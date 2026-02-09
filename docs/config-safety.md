# Config Safety: Self-Modifying Configuration

## Problem

Coop's agent has bash, read_file, write_file, and edit_file tools. It can already edit `coop.yaml`. The danger:

1. Agent writes bad config (typo, invalid model, wrong path)
2. Coop restarts (or a future hot-reload triggers)
3. Coop fails to start
4. Agent is unreachable — can't fix its own mistake
5. Human must SSH in and manually repair

This is the worst failure mode for an autonomous agent: a self-inflicted outage that it can't self-repair. Config errors are the #1 cause of production outages in systems like this.

## Current State

Config is loaded once at startup in `cmd_start`:

```
Config::load()           → parse YAML
config.resolve_workspace → check workspace dir exists
ensure provider == "anthropic"
AnthropicProvider::from_env → check API key env var
SignalChannel::connect   → check signal db
Gateway::new             → scan workspace index, build prompt
parse_cron               → validate cron expressions
```

If any of these fail, coop exits with an error. There is no validation command, no dry-run, no rollback. The agent can freely write broken YAML and the next restart will fail.

## Design Space

### Approach 1: `coop check` — Validation Command

**Modeled after:** `nginx -t`, `systemd-analyze verify`, `terraform validate`

Add a `coop check` subcommand that runs the full startup path without actually starting the server:

```
$ coop check
✓ config syntax valid
✓ workspace exists: ./workspaces/default
✓ provider: anthropic (model: claude-sonnet-4-20250514)
✓ API key: present (sk-ant-...xxx)
✓ workspace files: SOUL.md (287 tok), AGENTS.md (1204 tok)
✓ prompt builds successfully (2,891 / 30,000 tokens)
✓ users: alice (full), bob (inner)
✓ channels: signal (db: ./db/signal.db)
✓ cron: 2 entries valid
✓ all checks passed

$ echo $?
0
```

On failure:
```
$ coop check
✓ config syntax valid
✓ workspace exists: ./workspaces/default
✗ provider: unknown provider 'openai' (only 'anthropic' supported)
✗ cron[1] 'cleanup': invalid expression 'every day at 3am'

2 errors found
$ echo $?
1
```

**What it validates (in order):**

| Check | What breaks without it |
|-------|----------------------|
| YAML parse | Nothing starts |
| Required fields (agent.id, agent.model) | Panic or confusing error |
| Workspace dir exists | Gateway::new fails |
| Provider name supported | Hard bail in main.rs |
| API key env var present | AnthropicProvider::from_env fails |
| Model name plausible | API calls fail at runtime |
| Workspace files scan | Prompt builder errors |
| Prompt builds under budget | Truncated/broken system prompt |
| User configs valid | Trust routing errors |
| User match patterns well-formed | Messages route to wrong sessions |
| Signal db_path exists (if configured) | Channel fails to connect |
| Cron expressions parse | Scheduler skips entries |
| Cron user references exist | Warning: cron fires with wrong trust |
| Cron delivery channels valid | Warning: delivery silently fails |

**Output format:** Human-readable by default, `--json` flag for machine-readable (the agent parses this).

**Pros:**
- Simple, well-understood pattern
- Agent runs `coop check` after editing config via bash tool
- No new abstractions — reuses existing startup code
- Works today without hot-reload infrastructure

**Cons:**
- Advisory only — nothing prevents the agent from skipping the check
- Doesn't validate runtime behavior (will the API actually accept this model?)
- Separate process can't check things that require the running server's state

### Approach 2: Config Tool with Built-in Validation

Instead of the agent using edit_file + bash, provide a dedicated `config_edit` tool:

```json
{
  "name": "config_edit",
  "parameters": {
    "operation": "set | delete | append",
    "path": "agent.model",
    "value": "anthropic/claude-sonnet-4-20250514"
  }
}
```

The tool:
1. Reads current config
2. Applies the proposed change in memory
3. Runs full validation on the result
4. If valid → writes to disk, returns success + diff
5. If invalid → returns error, file unchanged

```
→ config_edit(path="agent.model", value="anthropic/nonexistent-model")
← ERROR: model 'anthropic/nonexistent-model' not recognized.
  Available: claude-sonnet-4-20250514, claude-opus-4-5-20251101, ...
  Config was NOT modified.
```

**Pros:**
- Atomic: validation and write are one operation, can't skip validation
- Structured: agent works with paths/values, not raw YAML editing
- Informative: returns diffs, shows what changed
- Safe: file is never written in an invalid state

**Cons:**
- New tool to maintain — config schema changes require tool updates
- Less flexible than direct file editing (complex nested changes are awkward)
- Doesn't help if the agent uses edit_file directly (can't prevent that)
- YAML path semantics for arrays are annoying (how do you reference "users[1].trust"?)

### Approach 3: Shadow Config + Promotion

Agent writes to `coop.staging.yaml`. A separate `coop promote` command validates and atomically swaps:

```
$ coop promote
Validating coop.staging.yaml...
✓ all checks passed
Diff:
  agent.model: claude-sonnet-4-20250514 → claude-opus-4-5-20251101
Backup: coop.yaml → coop.yaml.2026-02-07T22:30:00.bak
Applied: coop.staging.yaml → coop.yaml
Removed: coop.staging.yaml
```

The agent workflow:
1. `edit_file coop.staging.yaml` — write proposed config
2. `bash "coop promote --dry-run"` — see what would change
3. `bash "coop promote"` — apply if satisfied

**Pros:**
- Live config is never directly modified by the agent
- Dry-run shows exact diff before applying
- Automatic backup on promotion
- Clear separation: staging is a proposal, coop.yaml is truth

**Cons:**
- Two-file mental model is more complex
- Agent has to learn the staging workflow
- Nothing prevents the agent from editing coop.yaml directly anyway
- Promotion is still a cold check — doesn't verify the running server

### Approach 4: Hot Reload with Health Check

The running server watches coop.yaml for changes. On change:

```
coop.yaml modified
    │
    ▼
Parse + validate new config
    │
    ├── invalid → log error, keep running with old config
    │             inject system message: "Config change rejected: {reason}"
    │
    ├── valid → snapshot old config to .bak
    │           hot-swap: new provider, new routes, new cron
    │           │
    │           ├── health check passes → done
    │           │
    │           └── health check fails → rollback to .bak
    │                                    inject: "Config rolled back: {reason}"
    │
    └── agent sees the result in its next turn
```

Health check = send a synthetic message through the full pipeline and verify a response comes back within N seconds.

**Pros:**
- True runtime validation — actually tests the new config under load
- Automatic rollback — bad configs never stick
- Agent gets feedback without running a separate command
- Seamless: agent just edits the file, system handles the rest

**Cons:**
- Complex to implement — hot-swapping providers, channels, cron is nontrivial
- Health check is expensive (burns an API call)
- Race conditions: what if the agent sends a message during the swap?
- Requires careful state management for in-flight requests during transition
- Much more code to get right and test

### Approach 5: `coop check` + Write Hook (Hybrid)

Combine approaches 1 and 4 at a simpler level:

- `coop check` exists as a standalone command
- `coop.yaml` writes through a gateway-provided mechanism that auto-validates
- No hot-reload — just prevent bad writes and require restart

Implementation: a thin wrapper that intercepts writes to coop.yaml:

```rust
/// Validate, backup, and write config atomically.
pub fn safe_write_config(path: &Path, new_content: &str) -> Result<ConfigWriteResult> {
    // 1. Parse the new content
    let new_config: Config = serde_yaml::from_str(new_content)?;

    // 2. Run full validation
    let report = validate_config(&new_config)?;
    if !report.is_valid() {
        return Ok(ConfigWriteResult::Rejected(report));
    }

    // 3. Backup current config
    let backup_path = backup_config(path)?;

    // 4. Write new config atomically (write to .tmp, rename)
    atomic_write(path, new_content)?;

    Ok(ConfigWriteResult::Applied { backup: backup_path, report })
}
```

The agent uses this through a `config_write` tool or through `coop check --apply`:

```bash
# Agent edits config, then:
coop check                     # validate only
coop check --apply proposed.yaml  # validate + swap if valid
```

**Pros:**
- `coop check` is independently useful (CI, manual use, scripting)
- Atomic write prevents partial/corrupt config files
- Automatic backup on every change
- No hot-reload complexity — restart is explicit
- Agent can use the simpler `coop check` workflow via bash

**Cons:**
- Still requires restart to take effect (but restart is fast — under 2s)
- Agent could still edit coop.yaml with edit_file, bypassing the tool

## Recommendation

**Phase 1: `coop check` command (Approach 1)**

This is the highest-value, lowest-cost option. It:
- Catches the most common failures (bad YAML, missing workspace, invalid cron)
- Gives the agent a clear way to verify its changes
- Requires no architectural changes to the gateway
- Is useful for humans too (CI, manual config changes)
- Composes well — agent's workflow is `edit_file` → `bash "coop check"` → done

Implementation is straightforward: extract the validation logic already in `cmd_start` into a `validate_config` function, add a `Commands::Check` variant, wire it up.

**Phase 2: Automatic backup + atomic write**

Before any config write (whether by the agent or a human), the system:
- Copies `coop.yaml` → `coop.yaml.bak` (or timestamped)
- Writes new config via temp file + rename (no partial writes)

This is a small addition to phase 1 that prevents the "I can't get back to the old config" failure.

**Phase 3: `config_write` tool (Approach 2, simplified)**

Once `coop check` exists, wrap it in a tool:
```
config_write(content: "<full yaml>")
  → validates internally
  → backs up old config
  → writes atomically
  → returns structured result
```

This closes the "agent skips validation" gap. The tool refuses to write invalid configs. The agent can still use edit_file + bash if it wants, but the happy path is the safe tool.

**Phase 4: Hot reload (Approach 4) — only if needed**

Hot reload is complex and coop restarts in under 2 seconds. The restart-after-edit workflow is:
1. Agent edits config
2. Agent runs `coop check`
3. Agent tells user "I've updated my config — restarting"
4. Process restarts (systemd, Docker, manual)

This is fine for most cases. Hot reload only becomes necessary if coop manages long-lived stateful connections (WebSocket channels) that are expensive to re-establish.

## What `coop check` Validates

Detailed breakdown of the validation pipeline:

```rust
pub struct CheckReport {
    checks: Vec<CheckResult>,
}

pub struct CheckResult {
    name: String,         // "yaml_parse", "workspace", "provider", etc.
    passed: bool,
    message: String,      // "workspace exists: ./workspaces/default"
    severity: Severity,   // Error (blocks start) vs Warning (degraded)
}
```

### Hard Errors (coop will not start)

| Check | Validation |
|-------|-----------|
| `yaml_parse` | `serde_yaml::from_str` succeeds |
| `required_fields` | agent.id and agent.model present and non-empty |
| `workspace_exists` | `config.resolve_workspace()` succeeds |
| `provider_known` | provider.name is "anthropic" (or future supported) |
| `api_key_present` | If `provider.api_keys` is set: each entry has `env:` prefix and its env var is set. Otherwise: `ANTHROPIC_API_KEY` env var is set |

### Soft Errors (coop starts in degraded mode)

| Check | Validation |
|-------|-----------|
| `workspace_files` | Scan workspace, report which files exist/missing |
| `prompt_builds` | PromptBuilder succeeds, report token usage |
| `signal_db` | If signal configured, db_path exists |
| `cron_valid` | All cron expressions parse |
| `cron_users` | Cron user references match config users |
| `cron_delivery` | Delivery channels are supported |
| `user_patterns` | Match patterns are well-formed |

### Informational (always shown)

| Check | Output |
|-------|--------|
| `model_info` | Model name, provider |
| `user_count` | N users configured |
| `channel_count` | N channels configured |
| `cron_count` | N cron entries, next fire times |
| `prompt_budget` | Tokens used / budget at each trust level |
| `tool_count` | N tools available |

## Agent Workflow

The agent's system prompt (AGENTS.md) should include instructions like:

```markdown
## Config Changes

When modifying coop.yaml:
1. Read the current config with read_file
2. Make your changes with edit_file
3. Run `coop check` via bash to validate
4. If check fails, fix the errors and re-check
5. Never leave coop.yaml in an invalid state
```

Example agent interaction:
```
User: "Switch to opus"

Agent: [reads coop.yaml]
Agent: [edit_file: changes model to anthropic/claude-opus-4-5-20251101]
Agent: [bash: coop check]
→ ✓ all checks passed
  model: anthropic/claude-opus-4-5-20251101
  prompt: 2,891 / 30,000 tokens

Agent: "Done — I've updated my model to Claude Opus. The change will
        take effect on next restart."
```

Failed example:
```
User: "Add a cron job that runs every potato"

Agent: [reads coop.yaml]
Agent: [edit_file: adds cron entry with expr "every potato"]
Agent: [bash: coop check]
→ ✓ config syntax valid
  ✗ cron[2] 'new-job': invalid expression 'every potato'
  1 error found

Agent: [edit_file: reverts the cron entry]
Agent: "I couldn't add that — 'every potato' isn't a valid cron
        expression. Cron uses the format 'min hour day month weekday'.
        What schedule did you have in mind?"
```

## Non-Goals

- **Runtime config diffing.** We don't compare running config vs file config. The file is the source of truth, always.
- **Remote config management.** No API endpoint for config changes. Config lives on disk, modified by the agent or human.
- **Config encryption.** Secrets (API keys) stay in env vars, not in coop.yaml. The config file has no secrets to protect.
- **Schema versioning.** Config schema is simple enough that we don't need migration tooling. Breaking changes are handled by release notes.
