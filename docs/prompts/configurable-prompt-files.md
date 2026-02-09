# Configurable Prompt File Injection

Make the set of markdown files injected into the system prompt configurable via `coop.yaml`, with separate shared (workspace-level) and per-user file lists, sensible defaults, and auto-reloading on file change.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Today `default_file_configs()` in `crates/coop-core/src/prompt.rs` hardcodes seven files (SOUL.md, AGENTS.md, TOOLS.md, IDENTITY.md, USER.md, MEMORY.md, HEARTBEAT.md) all loaded from the workspace root. There is no way to change this set via config — adding, removing, or reordering files requires a code change.

The gateway calls `default_file_configs()` in two places: `Gateway::new()` (initial scan) and `Gateway::build_prompt()` (every turn refresh). The `WorkspaceIndex` already supports mtime-based refresh, so file content changes are detected. But the *list* of files is static.

The system also has no concept of "shared workspace files" vs "per-user workspace files." Per-user memory (`users/{user}/MEMORY.md`) is handled as a special case in `PromptBuilder::build_user_memory_layer()`, but there's no general mechanism for user-scoped prompt files.

## Goals

1. **Configurable file lists.** Operators can add, remove, or reorder prompt files via `coop.yaml` without code changes.
2. **Two scopes.** Shared files live in the workspace root. Per-user files live in `users/{user}/`. Each scope has its own default set.
3. **Sensible defaults.** When no `prompt` config is specified, the behavior matches what operators expect: shared SOUL.md, IDENTITY.md, TOOLS.md; per-user AGENTS.md, USER.md, TOOLS.md, HEARTBEAT.md.
4. **Auto-reload.** File content changes are detected via the existing mtime-based `WorkspaceIndex::refresh()`. Config changes to the file list are picked up by the existing config watcher.

## Design

### Config Schema

Add a `prompt` section to the gateway `Config`:

```yaml
prompt:
  shared_files:
    - path: SOUL.md
      trust: familiar
      cache: stable
      description: "Agent personality and voice"
    - path: IDENTITY.md
      trust: familiar
      cache: session
      description: "Agent identity"
    - path: TOOLS.md
      trust: full
      cache: session
      description: "Tool setup notes"
  user_files:
    - path: AGENTS.md
      trust: full
      cache: stable
      description: "Behavioral instructions"
    - path: USER.md
      trust: inner
      cache: session
      description: "Per-user info"
    - path: TOOLS.md
      trust: full
      cache: session
      description: "Per-user tool notes"
```

**Shared files** are resolved relative to the workspace root (e.g. `{workspace}/SOUL.md`).

**User files** are resolved relative to the user's directory (e.g. `{workspace}/users/{user}/AGENTS.md`). They are only loaded when a user is identified for the session.

Each entry has:
- `path` (required): Filename relative to its scope root.
- `trust` (optional, default `full`): Minimum trust level to see this file. Serialized as lowercase string matching `TrustLevel` variants.
- `cache` (optional, default `session`): Cache hint — `stable`, `session`, or `volatile`. Default `session`.
- `description` (optional, default derived from filename): One-line description for the memory index menu.

### Defaults

When `prompt` is absent from config, or when `shared_files`/`user_files` is absent, use these defaults:

**Default shared files:**
| path | trust | cache | description |
|------|-------|-------|-------------|
| SOUL.md | familiar | stable | Agent personality and voice |
| IDENTITY.md | familiar | session | Agent identity |
| TOOLS.md | full | session | Tool setup notes |

**Default user files:**
| path | trust | cache | description |
|------|-------|-------|-------------|
| AGENTS.md | full | stable | Behavioral instructions |
| USER.md | inner | session | Per-user info |
| TOOLS.md | full | session | Per-user tool notes |

These replace the current `default_file_configs()` which mixes both scopes into one flat list.

**Existing files that move:** MEMORY.md and HEARTBEAT.md are removed from the default prompt file lists. MEMORY.md is already handled by the memory prompt index system (`memory_prompt_index.rs`). HEARTBEAT.md is only relevant for cron sessions and should be loaded explicitly by cron config or added to a specific agent's config if needed. Operators who want them back can add them to their `shared_files` or `user_files`.

### Rust Types

In `crates/coop-gateway/src/config.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PromptConfig {
    #[serde(default = "default_shared_files")]
    pub shared_files: Vec<PromptFileEntry>,
    #[serde(default = "default_user_files")]
    pub user_files: Vec<PromptFileEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PromptFileEntry {
    pub path: String,
    #[serde(default = "default_file_trust")]
    pub trust: TrustLevel,
    #[serde(default = "default_file_cache")]
    pub cache: CacheHintConfig,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CacheHintConfig {
    Stable,
    Session,
    Volatile,
}
```

Add `prompt: PromptConfig` to `Config` with `#[serde(default)]`.

Add `impl Default for PromptConfig` that returns the default shared + user file lists.

Add a conversion method `PromptFileEntry::to_core(&self) -> coop_core::prompt::PromptFileConfig` to map from config types to the existing core types, defaulting `description` to the filename stem if not provided.

### Changes to `coop-core/src/prompt.rs`

1. **Keep `PromptFileConfig` as-is.** The core type remains the canonical representation. The gateway config types convert into it.

2. **Remove `default_file_configs()`.** It's no longer the source of truth — the gateway config provides the file list. Keep it available (maybe rename to `legacy_file_configs()`) only if needed for backward compatibility in tests, otherwise remove entirely.

3. **Add a `user_workspace` field to `PromptBuilder`:**

```rust
pub struct PromptBuilder {
    // ... existing fields ...
    user_file_configs: Vec<PromptFileConfig>,
}
```

Add a builder method:

```rust
#[must_use]
pub fn user_file_configs(mut self, configs: Vec<PromptFileConfig>) -> Self {
    self.user_file_configs = configs;
    self
}
```

4. **Modify `PromptBuilder::build()` to process user files.** After building shared file layers, if a user is set, resolve each user file config against `{workspace}/users/{user}/` and process them the same way as shared files (trust-gate, budget-check, truncate-or-overflow). This replaces the current `build_user_memory_layer()` special case.

5. **Update `WorkspaceIndex`** to support indexing files from multiple root directories. The simplest approach: give `WorkspaceIndex::scan()` and `refresh()` an additional parameter for user file configs + user workspace path, or call them twice (once for shared, once for user scope) and merge. Prefer the simplest approach that doesn't complicate the existing API.

    A clean option: `WorkspaceIndex` stores entries keyed by a *scoped path* like `shared:SOUL.md` or `user:AGENTS.md`, so shared and user files with the same filename (like TOOLS.md) don't collide. The display path in the menu should show the scope for clarity.

6. **Remove `build_user_memory_layer()`.** Per-user MEMORY.md is no longer a special case — if an operator wants it, they add `MEMORY.md` to `user_files` in their config.

### Changes to `crates/coop-gateway/src/gateway.rs`

1. **`Gateway::new()`**: Convert `config.prompt.shared_files` to `Vec<PromptFileConfig>` and pass to `WorkspaceIndex::scan()`. Store the converted configs (or derive them from `SharedConfig` each time).

2. **`Gateway::build_prompt()`**: On each call, load the current config snapshot, convert both `shared_files` and `user_files` to core types, pass shared configs to `WorkspaceIndex::refresh()`, and pass user configs to the `PromptBuilder`. When the config watcher updates the file lists, the next `build_prompt()` call automatically picks up the change.

3. **File list changes trigger re-scan.** If the file list in config changed since the last scan (compare the config-derived `Vec<PromptFileConfig>` to what was last scanned), call `WorkspaceIndex::scan()` instead of `refresh()` to fully rebuild the index. This handles files being added or removed from the config.

### Changes to `crates/coop-gateway/src/config_check.rs`

Add validation for the prompt config:
- Each `path` must be a relative path (no `..`, no absolute paths).
- `path` must not be empty.
- Warn if a shared file doesn't exist in the workspace (non-fatal — file may be created later).
- Warn on duplicate paths within the same scope.

### Changes to `crates/coop-gateway/src/config_watcher.rs`

The existing config watcher already handles `SharedConfig` hot-swap. The `prompt` field is not a restart-only field, so changes to file lists are picked up automatically.

Add `prompt` to `diff_sections()` so config reload logs which sections changed.

The `prompt` section should NOT be in `check_restart_only_fields()` — file list changes can be applied without restart.

### Auto-Reload Flow

The auto-reload of *file content* already works via `WorkspaceIndex::refresh()` which checks mtime on every `build_prompt()` call. No new file-watching infrastructure is needed.

The auto-reload of *file lists* (adding/removing files from config) works through the existing config watcher → `SharedConfig` update → next `build_prompt()` reads the new config snapshot.

Summary of the reload flow:
1. **File content changes** → `WorkspaceIndex::refresh()` detects mtime change → re-indexes → next prompt build uses new content.
2. **Config file list changes** → config watcher detects `coop.yaml` change → hot-swaps `SharedConfig` → next `build_prompt()` reads new file list → triggers `WorkspaceIndex::scan()` for full re-index.
3. **New user file appears** → `WorkspaceIndex::refresh()` detects the file on next call (was absent, now present) → indexes it → included in next prompt.

### Tracing

Add tracing for:
- `info!` when the prompt file list changes via config reload (log old count vs new count, which files were added/removed).
- `debug!` for each user file resolved (path, user, trust, tokens).
- Existing tracing in `WorkspaceIndex` and `PromptBuilder` already covers file inclusion/exclusion.

## Implementation Order

1. **Add config types.** Add `PromptConfig`, `PromptFileEntry`, `CacheHintConfig` to `config.rs` with defaults and serde. Add the `prompt` field to `Config`. Add conversion to `PromptFileConfig`.
2. **Update `prompt.rs`.** Add `user_file_configs` to `PromptBuilder`. Implement user file layer building. Update `WorkspaceIndex` to handle scoped paths. Remove `build_user_memory_layer()`. Remove or deprecate `default_file_configs()`.
3. **Update `gateway.rs`.** Wire config-derived file lists through `Gateway::new()` and `build_prompt()`. Detect file list changes and trigger re-scan.
4. **Update `config_check.rs`.** Add prompt config validation.
5. **Update `config_watcher.rs`.** Add `prompt` to `diff_sections()`.
6. **Update tests.** Update existing prompt builder tests to use explicit file configs instead of `default_file_configs()`. Add tests for: config round-trip (serialize/deserialize), user file resolution, scoped path collision (shared TOOLS.md vs user TOOLS.md), config change triggers re-scan, missing user directory is handled gracefully.
7. **Verify.** Run `just check`. Run with `COOP_TRACE_FILE=traces.jsonl` and confirm prompt file lists appear in traces. Test with a config that overrides defaults and verify the prompt content changes.

## Migration

Existing configs without a `prompt` section get the defaults automatically via `#[serde(default)]`. The defaults are chosen so that the most common files are included without config.

Operators who had custom files (unlikely since the list was hardcoded) just need to add a `prompt` section.

MEMORY.md and HEARTBEAT.md are no longer in the default list. If an operator was relying on MEMORY.md being injected directly (rather than through the memory index system), they need to add it to their `shared_files`. This is unlikely since the memory prompt index has been the recommended path.

## Example Configs

### Minimal (uses all defaults)
```yaml
agent:
  id: reid
  model: claude-sonnet-4-20250514
```

### Custom shared files only
```yaml
agent:
  id: reid
  model: claude-sonnet-4-20250514
prompt:
  shared_files:
    - path: SOUL.md
    - path: IDENTITY.md
    - path: TOOLS.md
    - path: CONTEXT.md
      description: "Project context and conventions"
```

### Full customization
```yaml
agent:
  id: reid
  model: claude-sonnet-4-20250514
prompt:
  shared_files:
    - path: SOUL.md
      trust: familiar
      cache: stable
    - path: IDENTITY.md
    - path: TOOLS.md
    - path: MEMORY.md
      trust: full
      cache: session
      description: "Long-term curated memory"
  user_files:
    - path: AGENTS.md
      cache: stable
    - path: USER.md
      trust: inner
    - path: TOOLS.md
    - path: PREFERENCES.md
      trust: inner
      description: "User preferences and settings"
```

### Empty user files (no per-user injection)
```yaml
prompt:
  user_files: []
```
