# Dynamic Skill Reload on Session Load

Rescan workspace skills on every `build_prompt()` call so that skills added to `{workspace}/skills/` while coop is running are available to new sessions without a restart. Active sessions already rebuild the system prompt each turn, so they pick up new skills automatically.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Skills are discovered by scanning `{workspace}/skills/*/SKILL.md` for YAML frontmatter with `name` and `description` fields. The scan function `scan_skills()` in `crates/coop-core/src/prompt.rs` reads the filesystem and returns `Vec<SkillEntry>`.

Currently, workspace-level skills are loaded **once** in `Gateway::new()`:

```rust
// crates/coop-gateway/src/gateway.rs, Gateway::new()
let skills = scan_skills(&workspace);
```

The result is stored as `skills: Vec<SkillEntry>` on the `Gateway` struct and cloned into every `PromptBuilder` call:

```rust
// crates/coop-gateway/src/gateway.rs, Gateway::build_prompt()
.skills(self.skills.clone())
```

This means skills added to the workspace after startup are invisible until coop is restarted.

Note: **per-user skills** (`users/{user}/skills/`) are already rescanned dynamically inside `PromptBuilder::build()` on every call. Only workspace-level skills have this staleness problem.

## Problem

If an operator drops a new `skills/foo/SKILL.md` into the workspace while coop is running, the skill never appears in the system prompt. This is inconsistent with how workspace files work — file *content* changes are detected via `WorkspaceIndex::refresh()` on every turn, but the skills list is frozen at startup.

## Design

Follow the same pattern as `WorkspaceIndex`: rescan skills inside `build_prompt()` and detect changes. The scan is cheap (readdir + small file reads), so doing it per-turn is fine.

### Option: Rescan in `build_prompt()`

Replace the static `skills: Vec<SkillEntry>` field with a `Mutex<Vec<SkillEntry>>` and rescan in `build_prompt()`, logging when skills change.

This is the simplest approach and consistent with how `workspace_index` already works in `build_prompt()`.

## Changes

### `crates/coop-gateway/src/gateway.rs`

1. **Change `skills` field type** from `Vec<SkillEntry>` to `Mutex<Vec<SkillEntry>>`.

2. **Update `Gateway::new()`** to wrap the initial scan in `Mutex::new(...)`.

3. **Update `Gateway::build_prompt()`** to rescan skills and detect changes:

```rust
// Inside build_prompt(), before building the PromptBuilder:
let current_skills = scan_skills(&self.workspace);
{
    let mut cached = self.skills.lock().expect("skills mutex poisoned");
    if cached.len() != current_skills.len()
        || cached.iter().zip(&current_skills).any(|(a, b)| a.name != b.name || a.path != b.path)
    {
        let added: Vec<&str> = current_skills
            .iter()
            .filter(|s| !cached.iter().any(|c| c.name == s.name))
            .map(|s| s.name.as_str())
            .collect();
        let removed: Vec<&str> = cached
            .iter()
            .filter(|c| !current_skills.iter().any(|s| s.name == c.name))
            .map(|c| c.name.as_str())
            .collect();
        info!(
            added = ?added,
            removed = ?removed,
            total = current_skills.len(),
            "workspace skills changed"
        );
        *cached = current_skills.clone();
    }
}

// Then use current_skills when building:
let mut builder = PromptBuilder::new(...)
    // ...
    .skills(current_skills);
```

The key points:
- Rescan via `scan_skills(&self.workspace)` every call (cheap filesystem op).
- Compare against the cached list — log `info!` only when skills actually change.
- Update the cached list when changes are detected.
- Pass the fresh list to `PromptBuilder`.

### Cache impact

**No cache impact.** The system prompt is rebuilt from scratch on every `build_prompt()` call. Anthropic's prefix caching operates on the stable/session blocks — the skills layer uses `CacheHint::Stable`, so it lives in the stable prefix block. When skills change, the stable prefix changes, which naturally invalidates the cache for subsequent turns. Active sessions that don't add/remove skills see no cache difference because the skills content is identical.

### Tracing

- `info!` when skills change (added/removed names, new total count) — this is a significant event operators want to see.
- The existing `debug!` in `scan_skills_with_prefix()` already logs each discovered skill, so individual skill discovery is traced.

## Tests

### Unit test in `crates/coop-gateway/src/gateway.rs` (or a separate test module)

Add a test that:
1. Creates a temp workspace with no skills.
2. Creates a `Gateway`.
3. Runs a turn (or calls `build_prompt()` directly if accessible) — verifies no skills in prompt.
4. Adds `skills/test-skill/SKILL.md` to the workspace with valid frontmatter.
5. Runs another turn — verifies the new skill appears in the system prompt.

Since `build_prompt()` is async and private, the test should go through `run_turn_with_trust` like the existing gateway tests. Alternatively, if adding a `pub(crate)` accessor for testing is simpler, that's fine too.

### Existing tests

Verify no existing tests break — the change is backward-compatible since skills loaded at startup will still be there. Run:
```bash
cargo test -p coop-gateway
cargo test -p coop-core
```

## Implementation Order

1. Change `skills` field to `Mutex<Vec<SkillEntry>>` in `Gateway` struct.
2. Update `Gateway::new()` to wrap in `Mutex`.
3. Update `build_prompt()` to rescan and detect changes.
4. Add test.
5. Run `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`.
6. Verify with `COOP_TRACE_FILE=traces.jsonl` — add a skill while running, confirm the "workspace skills changed" event appears in traces on next turn.


## Docs

Be sure to update readme docs or any other related docs about this behavior.

## Not in Scope

- **File-watching for skills.** Rescanning on each `build_prompt()` call is sufficient. Skills are a small directory and `readdir` is cheap.
- **Removing skills from active sessions mid-turn.** Skills are only read at prompt build time. If a skill is removed between iterations of a multi-iteration turn, it stays in the prompt for that turn. This is fine.
- **Per-user skill staleness.** Per-user skills are already rescanned on every `PromptBuilder::build()` call — no change needed.
