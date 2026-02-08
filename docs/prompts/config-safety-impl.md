# Config Safety — Implementation Prompt (Phases 1–3)

Read `docs/config-safety.md` for full design rationale. Read `AGENTS.md` for project rules. Follow the development loop: fmt → build → test → lint.

## Overview

Implement three features:

1. **`coop check`** — CLI subcommand that validates config without starting the server
2. **Automatic backup + atomic write** — helper functions for safe config writes
3. **`config_write` tool** — agent tool that validates, backs up, and atomically writes config

All three build on the same validation core. Implement them in order — each phase depends on the prior.

---

## Phase 1: `coop check`

### 1a. Create `crates/coop-gateway/src/config_check.rs`

This module contains the validation pipeline and report types. Keep it in `coop-gateway` (not `coop-core`) because it depends on gateway-specific knowledge (provider names, cron parsing, signal channel config). Making it `pub(crate)` is fine.

**Types:**

```rust
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Error,   // coop will not start
    Warning, // coop starts degraded
    Info,    // always shown
}

#[derive(Debug, Clone)]
pub(crate) struct CheckResult {
    pub name: &'static str,
    pub severity: Severity,
    pub passed: bool,
    pub message: String,
}

#[derive(Debug, Default)]
pub(crate) struct CheckReport {
    pub results: Vec<CheckResult>,
}
```

**`CheckReport` methods:**

- `push(&mut self, result: CheckResult)` — add a result
- `has_errors(&self) -> bool` — any `Severity::Error` with `passed == false`
- `has_warnings(&self) -> bool` — any `Severity::Warning` with `passed == false`
- `print_human(&self)` — print results to stdout, using `✓` for passed, `✗` for failed, `⚠` for warnings, `·` for info. Print a summary line at the end: "N errors, M warnings" or "all checks passed"
- `print_json(&self)` — print results as a JSON object: `{"passed": bool, "errors": N, "warnings": N, "checks": [{"name": "...", "severity": "...", "passed": bool, "message": "..."}]}`

**`validate_config` function:**

```rust
pub(crate) fn validate_config(
    config_path: &Path,
    config_dir: &Path,
) -> CheckReport
```

This function runs all checks in order, collecting results. It must never panic or return `Result::Err` — every failure becomes a `CheckResult` in the report. This is critical: the check command must always produce output, even for catastrophically broken configs.

**Checks to run (in order):**

1. **`yaml_parse`** (Error): Try `Config::load(config_path)`. If it fails, record the error and return early — no point running further checks on an unparseable config.

2. **`required_fields`** (Error): Check `config.agent.id` is non-empty and `config.agent.model` is non-empty.

3. **`workspace_exists`** (Error): Call `config.resolve_workspace(config_dir)`. Record pass/fail.

4. **`provider_known`** (Error): Check `config.provider.name == "anthropic"`. Include the actual value in the message.

5. **`api_key_present`** (Error): Check `std::env::var("ANTHROPIC_API_KEY").is_ok()`. Do NOT log the key value — just whether it's present.

6. **`workspace_files`** (Info): If workspace resolved, call `WorkspaceIndex::scan()` with `default_file_configs()`. List each file found with its token count. Uses `coop_core::prompt::{WorkspaceIndex, default_file_configs}`.

7. **`prompt_builds`** (Warning): If workspace resolved, try `PromptBuilder::new(...).trust(TrustLevel::Full).build(&index)`. Report total tokens / budget. Uses `coop_core::prompt::PromptBuilder`.

8. **`users`** (Info): Report user count and names. Check for duplicate user names (Warning).

9. **`signal_channel`** (Warning): If `config.channels.signal` is configured, check that the db_path file/directory exists on disk. Resolve relative paths against `config_dir` using the same `resolve_config_path` helper that `cmd_start` uses (in `tui_helpers.rs`). Don't try to connect — just check the path exists.

10. **`cron_expressions`** (Warning): For each cron entry, try to parse the expression using the same `parse_cron` logic from `scheduler.rs`. Record pass/fail per entry. **You'll need to make `parse_cron` `pub(crate)`** — it's currently private in `scheduler.rs`.

11. **`cron_users`** (Warning): For each cron entry with a `user` field, check the user exists in `config.users`. Warn if not.

12. **`cron_delivery`** (Warning): For each cron entry with `deliver`, check that `deliver.channel` is "signal" (only supported channel). Warn if not.

### 1b. Add `Check` to CLI

In `crates/coop-gateway/src/cli.rs`, add a `Check` variant to `Commands`:

```rust
Check {
    /// Output format: human (default) or json
    #[arg(long, default_value = "human")]
    format: String,
},
```

### 1c. Wire up in `main.rs`

Add `mod config_check;` to the module list.

In `main()`, add the match arm:

```rust
Commands::Check { format } => cmd_check(cli.config.as_deref(), &format),
```

Note: `cmd_check` is a **sync** function (not async). None of the validation checks need async. This avoids pulling in tokio for a simple validation command. Signature:

```rust
fn cmd_check(config_path: Option<&str>, format: &str) -> Result<()> {
    let config_file = Config::find_config_path(config_path);
    let config_dir = config_file
        .parent()
        .unwrap_or(&PathBuf::from("."))
        .to_path_buf();

    let report = config_check::validate_config(&config_file, &config_dir);

    match format {
        "json" => report.print_json(),
        _ => report.print_human(),
    }

    if report.has_errors() {
        std::process::exit(1);
    }
    Ok(())
}
```

Update `console_log` to include `Commands::Check { .. }` (it should NOT enable console logging — check output goes to stdout, not tracing).

### 1d. Make `parse_cron` accessible

In `crates/coop-gateway/src/scheduler.rs`, change `fn parse_cron` from `fn` to `pub(crate) fn`. No other changes to that file.

### 1e. Make `resolve_config_path` accessible

The `tui_helpers::resolve_config_path` function resolves relative paths against a config directory. Check if it's already `pub(crate)`. If not, make it `pub(crate)`.

### 1f. Tests

Add tests in `crates/coop-gateway/src/config_check.rs` (inline `#[cfg(test)] mod tests`):

- `test_valid_minimal_config` — Create a tempdir with a valid coop.yaml and workspace. Run `validate_config`. Assert no errors.
- `test_invalid_yaml` — Write garbage to a file. Assert `yaml_parse` fails and report has errors.
- `test_missing_workspace` — Valid YAML but workspace dir doesn't exist. Assert `workspace_exists` fails.
- `test_unknown_provider` — Config with `provider.name: "openai"`. Assert `provider_known` fails.
- `test_invalid_cron` — Config with a bad cron expression. Assert the cron check fails as a warning.
- `test_cron_user_not_in_config` — Cron references user "mallory" who isn't in users. Assert warning.
- `test_report_has_errors` — Unit test for `CheckReport::has_errors()`.
- `test_report_json_output` — Verify `print_json` produces parseable JSON (capture stdout or build the JSON value directly and assert on it).

All tests use tempfile for workspace dirs. Use only placeholder data per AGENTS.md rules.

---

## Phase 2: Automatic Backup + Atomic Write

### 2a. Add to `config_check.rs` (or a new `config_write.rs` — your call on file size)

If `config_check.rs` will exceed ~300 lines with phases 1+2, create `crates/coop-gateway/src/config_write.rs` for the write helpers. Otherwise keep them in `config_check.rs`. The write functions are:

```rust
use std::path::{Path, PathBuf};
use anyhow::Result;

/// Back up the config file to `{path}.bak`.
/// Overwrites any existing .bak file.
/// Returns the backup path.
pub(crate) fn backup_config(path: &Path) -> Result<PathBuf> {
    let backup = path.with_extension("yaml.bak");
    std::fs::copy(path, &backup)?;
    Ok(backup)
}

/// Write content to a file atomically: write to a .tmp sibling, then rename.
/// This prevents partial/corrupt writes if the process crashes mid-write.
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Validate new config content, back up the old file, and write atomically.
/// Returns the check report. The caller should inspect `report.has_errors()`
/// to determine if the write was applied or rejected.
pub(crate) fn safe_write_config(
    config_path: &Path,
    new_content: &str,
) -> (CheckReport, Option<PathBuf>) {
    // 1. Write new content to a temp file and validate it
    //    We need a file on disk for validate_config to read.
    let staging = config_path.with_extension("yaml.staging");
    if let Err(e) = std::fs::write(&staging, new_content) {
        let mut report = CheckReport::default();
        report.push(CheckResult {
            name: "write_staging",
            severity: Severity::Error,
            passed: false,
            message: format!("failed to write staging file: {e}"),
        });
        return (report, None);
    }

    let config_dir = config_path.parent().unwrap_or(Path::new("."));
    let report = validate_config(&staging, config_dir);

    // Clean up staging file
    let _ = std::fs::remove_file(&staging);

    if report.has_errors() {
        return (report, None);
    }

    // 2. Back up current config (if it exists)
    let backup = if config_path.exists() {
        match backup_config(config_path) {
            Ok(p) => Some(p),
            Err(e) => {
                let mut r = report; // move report to mutate, but we already checked no errors
                // Actually, reconstruct — can't mutate after move easily.
                // Simpler: just use a new report with the backup error.
                let mut err_report = CheckReport::default();
                err_report.results = r.results;
                err_report.push(CheckResult {
                    name: "backup",
                    severity: Severity::Error,
                    passed: false,
                    message: format!("failed to backup config: {e}"),
                });
                return (err_report, None);
            }
        }
    } else {
        None
    };

    // 3. Write atomically
    if let Err(e) = atomic_write(config_path, new_content) {
        // Failed to write — report stays valid (config unchanged)
        let mut err_report = CheckReport { results: report.results };
        err_report.push(CheckResult {
            name: "atomic_write",
            severity: Severity::Error,
            passed: false,
            message: format!("failed to write config: {e}"),
        });
        return (err_report, backup);
    }

    (report, backup)
}
```

NOTE: The above is pseudocode showing intent. The actual implementation should be cleaner — just use `&mut report` throughout rather than moving. Keep `CheckReport.results` as a `Vec` and just push to it. The key contract: if `report.has_errors()` is true after `safe_write_config`, the original config file was NOT modified (except that a .bak may exist).

### 2b. Tests

- `test_backup_config` — Write a config file, call `backup_config`, verify .bak exists with same content.
- `test_atomic_write` — Call `atomic_write`, verify file has new content, verify no .tmp file remains.
- `test_safe_write_valid_config` — Full roundtrip: write a valid config via `safe_write_config`, verify the file is updated and .bak exists.
- `test_safe_write_invalid_config` — Pass broken YAML to `safe_write_config`, verify original file is unchanged.
- `test_safe_write_invalid_provider` — Pass valid YAML with `provider.name: "openai"`, verify original file is unchanged.

---

## Phase 3: `config_write` Tool

### 3a. Create `crates/coop-gateway/src/config_tool.rs`

This is an agent tool (implements `coop_core::Tool`) that wraps `safe_write_config`. It lives in `coop-gateway` because it depends on the gateway's validation logic.

**Tool definition:**

```rust
use coop_core::traits::{Tool, ToolContext};
use coop_core::types::{ToolDef, ToolOutput, TrustLevel};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use tracing::info;

#[derive(Debug)]
pub(crate) struct ConfigWriteTool {
    config_path: PathBuf,
}

impl ConfigWriteTool {
    pub(crate) fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}
```

**Schema:**

```json
{
  "type": "object",
  "properties": {
    "content": {
      "type": "string",
      "description": "Complete YAML content for coop.yaml. Must be the full file — not a patch or partial update. The content is validated before writing. If validation fails, the file is not modified."
    }
  },
  "required": ["content"]
}
```

**`Tool` implementation:**

- Tool name: `"config_write"`
- Description: `"Validate and write coop.yaml. Backs up the current config before writing. Returns validation results. If any errors are found, the file is NOT modified."`
- Trust gate: require `TrustLevel::Full` (only full-trust users can modify config). Return `ToolOutput::error("config_write requires Full trust level")` otherwise.
- Extract `content` string from arguments.
- Call `safe_write_config(&self.config_path, &content)`.
- If `report.has_errors()`: return `ToolOutput::error(format!("Config validation failed. File was NOT modified.\n\n{}", report.to_summary_string()))`.
- If no errors: return `ToolOutput::success(format!("Config written successfully. Backup: {}\n\n{}", backup_path, report.to_summary_string()))`.
- Log with `info!` on both success and failure.

Add a `to_summary_string(&self) -> String` method to `CheckReport` that produces a compact text summary (similar to `print_human` but returns a string instead of printing). Both `print_human` and `to_summary_string` can share the same formatting logic.

### 3b. Register the tool

The `ConfigWriteTool` needs to be added to the tool executor in `main.rs`'s `cmd_start` function. The existing pattern uses `DefaultExecutor` (from `coop-core`) and optionally `CompositeExecutor` when Signal tools are present.

Add `ConfigWriteTool` similarly: construct it with the config file path, then include it in the executor. The cleanest way:

In `cmd_start`, after constructing `default_executor`:
- Create `ConfigWriteTool::new(config_file.clone())`.
- Wrap it in a `SimpleExecutor` (or create a small `SingleToolExecutor` wrapper).

Actually, the simplest approach: make `DefaultExecutor` accept additional tools, OR use `CompositeExecutor` always (even without Signal). Look at how `CompositeExecutor` is already used in `cmd_start` — it wraps multiple `Box<dyn ToolExecutor>`. Create a minimal executor wrapper for the config tool:

```rust
// In config_tool.rs
pub(crate) struct ConfigToolExecutor {
    tool: ConfigWriteTool,
}

impl ConfigToolExecutor {
    pub(crate) fn new(config_path: PathBuf) -> Self {
        Self {
            tool: ConfigWriteTool::new(config_path),
        }
    }
}

#[async_trait]
impl ToolExecutor for ConfigToolExecutor {
    async fn execute(&self, name: &str, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if name == "config_write" {
            self.tool.execute(arguments, ctx).await
        } else {
            Ok(ToolOutput::error(format!("unknown tool: {name}")))
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![self.tool.definition()]
    }
}
```

Then in `cmd_start`, always use `CompositeExecutor`:

```rust
let mut executors: Vec<Box<dyn ToolExecutor>> = vec![
    Box::new(default_executor),
    Box::new(ConfigToolExecutor::new(config_file.clone())),
];

// ... add signal executor if present ...

let executor: Arc<dyn ToolExecutor> = Arc::new(CompositeExecutor::new(executors));
```

**Important:** Also register the tool in `cmd_chat` (the standalone TUI mode), not just `cmd_start`. The `cmd_chat` function also constructs its own executor and gateway. Apply the same pattern there.

NOTE: `CompositeExecutor` is in `coop-core` (`coop_core::tools::CompositeExecutor`). It's currently only imported behind `#[cfg(feature = "signal")]` in `main.rs`. Change it to an unconditional import since it's now always needed.

### 3c. Tests

Add tests in `crates/coop-gateway/src/config_tool.rs`:

- `test_config_write_valid` — Construct a `ConfigWriteTool` pointing at a temp config file. Write valid YAML. Verify file is updated, .bak exists.
- `test_config_write_invalid_yaml` — Write garbage. Verify file is unchanged, output contains error.
- `test_config_write_trust_gate` — Call with `TrustLevel::Public`. Verify rejection.
- `test_config_write_missing_workspace` — Write valid YAML that references a nonexistent workspace. Verify file is unchanged (workspace_exists is an Error-severity check).

---

## File Inventory

New files:
- `crates/coop-gateway/src/config_check.rs` — validation pipeline + report types + backup/write helpers
- `crates/coop-gateway/src/config_tool.rs` — config_write tool + executor wrapper

Modified files:
- `crates/coop-gateway/src/cli.rs` — add `Check` variant
- `crates/coop-gateway/src/main.rs` — add `mod config_check; mod config_tool;`, add `cmd_check`, wire up `ConfigToolExecutor` in both `cmd_start` and `cmd_chat`, make `CompositeExecutor` import unconditional
- `crates/coop-gateway/src/scheduler.rs` — make `parse_cron` `pub(crate)`
- `crates/coop-gateway/src/tui_helpers.rs` — make `resolve_config_path` `pub(crate)` if not already

No changes to `coop-core` or any other crate. No new dependencies.

---

## Constraints

- No new crate dependencies. Everything needed is already available (`serde_json` for JSON output, `cron` for parsing, `tempfile` for tests).
- Keep `config_check.rs` under 500 lines. If it goes over, split backup/write helpers into `config_write.rs`.
- Keep `config_tool.rs` under 200 lines.
- All output from `coop check` goes to stdout (not tracing). The check command should NOT initialize tracing subscribers — it's a pure validation tool.
- `validate_config` never panics or returns `Err`. Every failure is a `CheckResult`.
- Exit code: 0 if no errors, 1 if any errors. Warnings don't affect exit code.
- Tests use `tempfile::tempdir()` and fake data only (alice, bob, etc.).

## Development Loop

```bash
# After each phase:
cargo fmt
cargo build
cargo test -p coop-gateway
cargo clippy --all-targets --all-features -- -D warnings
```

Verify manually:
```bash
# Phase 1 — should pass:
cargo run -- check

# Phase 1 — should fail:
echo "garbage" > /tmp/bad.yaml
cargo run -- -c /tmp/bad.yaml check

# Phase 3 — visible in tool list (grep for config_write in debug output)
```
