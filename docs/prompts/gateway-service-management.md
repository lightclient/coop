# Gateway Service Management

Implement `coop gateway` subcommands for managing coop as a long-running background service on macOS (launchd) and Linux (systemd). The goal: users run `coop gateway install` once, and the agent is always available — surviving logouts, reboots, and crashes.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Background

Coop already has a client-server split:

- `coop start` — foreground daemon. Listens on a Unix socket (`$XDG_RUNTIME_DIR/coop/<agent>.sock` or `/tmp/coop-<agent>.sock`), runs Signal loop, cron scheduler, memory maintenance, accepts IPC clients.
- `coop attach` — TUI client that connects to the running daemon over IPC.
- `coop chat` — standalone TUI with embedded gateway (no daemon).

The IPC protocol (`coop-ipc` crate) uses newline-delimited JSON over Unix domain sockets. The existing `coop start` is the daemon entry point — service management wraps it.

What's missing: lifecycle management that makes the daemon feel invisible. Users shouldn't need to manually start/stop the daemon or remember to restart it after a reboot.

## Subcommands

```
coop gateway install     Install + enable service (and start immediately unless --no-start)
coop gateway uninstall   Disable and remove the system service
coop gateway start       Start the gateway (via service manager if installed, otherwise direct)
coop gateway stop        Stop the gateway
coop gateway restart     Restart the gateway
coop gateway rollback    Restore config backup and optionally restart gateway
coop gateway status      Show gateway status
coop gateway logs        Show recent gateway logs (use -f to follow)
```

All subcommands accept the global `--config` flag to identify which agent configuration to use. The agent ID from the config determines the service name.

### CLI structure

Add a `Gateway` variant to `Commands` in `cli.rs`:

```rust
#[derive(Subcommand)]
pub(crate) enum Commands {
    // ... existing variants ...
    Gateway {
        #[command(subcommand)]
        command: GatewayCommands,
    },
}

#[derive(Subcommand)]
pub(crate) enum GatewayCommands {
    Install {
        /// Extra environment variables to persist for the gateway service (KEY=VALUE).
        /// API key variables (ANTHROPIC_API_KEY, etc.) are captured automatically
        /// from the current environment if set.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        envs: Vec<String>,

        /// Override COOP_TRACE_FILE for the installed service.
        #[arg(long, value_name = "PATH")]
        trace_file: Option<String>,

        /// Override COOP_TRACE_MAX_SIZE for the installed service.
        #[arg(long, value_name = "BYTES")]
        trace_max_size: Option<u64>,

        /// Override RUST_LOG for the installed service.
        #[arg(long, value_name = "FILTER")]
        rust_log: Option<String>,

        /// Install + enable, but do not start immediately.
        #[arg(long)]
        no_start: bool,

        /// Print generated service config/script instead of installing.
        /// Useful on systems without systemd/launchd (OpenRC, runit, etc.)
        /// or when you want to customize files before installation.
        #[arg(long)]
        print: bool,

        /// When used with --print, include real secret values in env previews.
        /// Default is redacted output.
        #[arg(long, requires = "print")]
        print_secrets: bool,
    },
    Uninstall,
    Start,
    Stop,
    Restart,
    Rollback {
        /// Backup config path to restore. Defaults to `<config>.bak` layout
        /// used by config_write (`coop.toml.bak`).
        #[arg(long, value_name = "PATH")]
        backup: Option<String>,

        /// Restore config only. Do not restart gateway.
        #[arg(long)]
        no_restart: bool,

        /// Seconds to wait for gateway socket health after restart.
        #[arg(long, default_value = "10")]
        wait_seconds: u64,
    },
    Status,
    Logs {
        /// Number of recent lines to print.
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,

        /// Follow log output (like tail -f).
        #[arg(short, long)]
        follow: bool,
    },
}
```

### Dispatching in main.rs

```rust
Commands::Gateway { command } => cmd_gateway(cli.config.as_deref(), command).await,
```

The `cmd_gateway` function loads config (for agent ID and workspace path) and delegates to the service module.

### Install-time configuration

`gateway install` should resolve effective service environment in this precedence order:

1. CLI flags (`--trace-file`, `--trace-max-size`, `--rust-log`, `--env`)
2. Current process env vars (for captured key refs)
3. Defaults (`COOP_TRACE_FILE=<config_dir>/traces.jsonl`, no explicit `COOP_TRACE_MAX_SIZE`, default `RUST_LOG` behavior)

Persist the resolved environment so future `gateway start/restart` uses the same values even when run from a shell without API keys exported.

## Service module

Create `crates/coop-gateway/src/service.rs`. This module contains all platform-specific service management logic.

### Platform detection

```rust
enum Platform {
    Systemd,
    Launchd,
    /// No recognized service manager. The gateway can still be started
    /// via direct background spawn (PID file + manual lifecycle), but
    /// `install` can only `--print` the service configuration.
    Unsupported,
}

fn detect_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Launchd
    } else if cfg!(target_os = "linux")
        && Path::new("/run/systemd/system").exists()
    {
        // /run/systemd/system is the canonical detection method
        // used by systemd itself (sd_booted).
        Platform::Systemd
    } else {
        Platform::Unsupported
    }
}
```

**`Platform::Unsupported` is not an error.** The `start`/`stop`/`restart`/`status` commands all work via the fallback path (direct background spawn + PID file). Only `install`/`uninstall` require a recognized service manager — and `install --print` works everywhere.

### Service identity

The service name is derived from the agent ID in the config:

- **systemd:** `coop-<agent_id>.service` (user unit)
- **launchd:** `com.coop.<agent_id>` (user agent)

Sanitize the agent ID the same way `socket_path` does — replace non-alphanumeric characters (except `-` and `_`) with `-`.

### Paths

```rust
struct ServicePaths {
    /// Path to the coop binary (`std::env::current_exe`).
    binary: PathBuf,
    /// Absolute path to the config file.
    config: PathBuf,
    /// Where the system unit/plist file lives.
    unit_file: PathBuf,
    /// Service environment file (contains COOP_TRACE_* + API keys), mode 0600.
    env_file: PathBuf,
    /// Optional launchd wrapper script that sources env_file then execs coop.
    launchd_wrapper: PathBuf,
    /// Trace file path for COOP_TRACE_FILE.
    trace_file: PathBuf,
    /// Stdout log path (for launchd, which doesn't have journald).
    stdout_log: PathBuf,
    /// Stderr log path.
    stderr_log: PathBuf,
}
```

Default trace file: `<config_dir>/traces.jsonl` (next to `coop.toml`). This is also where the existing `just trace` recipes expect it.

Log directory for launchd stdout/stderr: `<config_dir>/logs/`.

Service metadata directory: `<config_dir>/service/` (unit/plist copy references, env file, wrapper script).

### PID file

Add PID file support to `coop start` itself so lifecycle commands work even without a service manager.

Place shared PID helpers in `service.rs` and call them from both `cmd_start` and `cmd_gateway` handlers.

On startup, `cmd_start` writes `$XDG_RUNTIME_DIR/coop/<agent_id>.pid` (same directory as the socket) containing the PID as a decimal string. On clean shutdown, it removes the file. On startup, if a stale PID file exists (the process is not running — check with `kill(pid, 0)`), log a warning and remove it.

This gives `gateway status` and `gateway stop` something to check even without systemd/launchd.

```rust
fn write_pid_file(agent_id: &str) -> Result<PathBuf> { ... }
fn read_pid_file(agent_id: &str) -> Option<u32> { ... }
fn remove_pid_file(agent_id: &str) { ... }
fn is_pid_alive(pid: u32) -> bool { ... }
```

---

## Platform: systemd (Linux)

### Install

Generate `~/.config/systemd/user/coop-<agent_id>.service`:

```ini
# Managed by coop — do not edit manually.
# Reinstall with: coop gateway install

[Unit]
Description=Coop Agent Gateway ({agent_id})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={binary} start --config {config}
Restart=on-failure
RestartSec=5
EnvironmentFile={env_file}

[Install]
WantedBy=default.target
```

Write `{env_file}` with mode `0600` (owner read/write only). Include:

- `COOP_TRACE_FILE`
- `COOP_TRACE_MAX_SIZE` (if configured)
- `RUST_LOG` (if configured)
- Captured provider/secrets env vars
- Additional `--env KEY=VALUE` vars

After writing the unit + env files:

1. `systemctl --user daemon-reload`
2. `systemctl --user enable coop-<agent_id>.service`
3. Unless `--no-start`, start immediately with `systemctl --user start coop-<agent_id>.service`
4. Enable lingering so the service survives logout and starts at boot:
   - Run `loginctl enable-linger $USER`. This requires either root or polkit authorization.
   - If it succeeds, print confirmation: `lingering enabled (gateway will start at boot)`.
   - If it fails (non-zero exit — user lacks privileges), print a clear warning and the manual command:

```
⚠ Could not enable lingering (requires sudo or polkit).
  Without it, the gateway will stop when you log out.
  Run manually: sudo loginctl enable-linger $USER
```

   This is the key difference vs macOS: launchd agents start at login automatically, but systemd user services require lingering to survive logout and start at boot. The install command must attempt to enable it — don't just warn.

5. Print success message with the service name and how to start it.

**Captured environment variables:** On install, automatically capture these from the current environment if they are set:

- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY` (for memory embeddings)
- Any variable referenced by `provider.api_keys` entries (`env:VAR_NAME` → capture `VAR_NAME`)
- Variables passed via `--env`

Do NOT capture `HOME`, `USER`, `PATH`, `XDG_*` — systemd sets those correctly for user services.

**Secret-handling requirement:** Do not inline secrets in the systemd unit itself. Persist all sensitive values in `{env_file}` with mode `0600`. Print a note after install indicating where secrets were stored and reminding users to protect backups of that file.

### Uninstall

1. `systemctl --user stop coop-<agent_id>.service` (ignore error if not running)
2. `systemctl --user disable coop-<agent_id>.service`
3. Remove the unit file
4. Remove `{env_file}`
5. `systemctl --user daemon-reload`
6. Print confirmation

### Start / Stop / Restart

If the service unit file exists:
- start: `systemctl --user start coop-<agent_id>.service`
- stop: `systemctl --user stop coop-<agent_id>.service`
- restart: `systemctl --user restart coop-<agent_id>.service`

If the service is NOT installed, `gateway start` falls back to spawning `coop start` as a background process (see Fallback section). `gateway stop` falls back to PID file + SIGTERM. `gateway restart` is stop + start.

### Rollback (all platforms)

`coop gateway rollback` restores the main config from backup and (by default) restarts the gateway.

Purpose: make self-healing scripts safe when the agent edits config. Typical flow:

1. Save backup (already done by `config_write` as `coop.toml.bak`)
2. Apply config change + restart
3. Health-check gateway socket
4. If unhealthy, run rollback

Example automation pattern:

```bash
# apply config mutation (tool/script)
coop gateway restart
if ! coop gateway status >/dev/null; then
  coop gateway rollback --wait-seconds 20
fi
```

Default behavior:

1. Resolve backup path:
   - `--backup <path>` if provided
   - otherwise `<config_path>.bak` layout from `config_write` (`coop.toml.bak`)
2. Verify backup exists and validates with `validate_config`
3. Atomically replace current config with backup (`config_write::atomic_write` + temp read)
4. Save current broken config to `<config>.failed-<timestamp>.toml` for forensics
5. Unless `--no-restart`, run `gateway restart`
6. Wait up to `--wait-seconds` for socket health (`Hello` handshake)

If health-check fails after rollback restart, return non-zero and print both paths:

- restored backup path
- saved failed config path

This gives automation a deterministic failure signal while still restoring the last-known-good config.

### Status

```
$ coop gateway status

🐔 coop gateway — agent "reid"
  status:   running (pid 12345)
  service:  coop-reid.service (systemd user)
  uptime:   2d 4h 15m
  socket:   /run/user/1000/coop/reid.sock
  config:   /home/alice/.coop/coop.toml
  traces:   /home/alice/.coop/traces.jsonl
```

Gather from:
- PID file: whether the process is alive
- Socket path: whether the socket exists and is connectable (try a Hello handshake)
- `systemctl --user is-active`: service state (if installed)
- `systemctl --user show ... --property=ActiveEnterTimestamp`: uptime (if running via systemd)
- If no systemd: use PID file creation time or `/proc/<pid>/stat` start time for uptime

If the gateway is not running:
```
🐔 coop gateway — agent "reid"
  status:   stopped
  service:  coop-reid.service (systemd user, enabled)
  socket:   /run/user/1000/coop/reid.sock (stale)
  config:   /home/alice/.coop/coop.toml
```

If no service is installed:
```
🐔 coop gateway — agent "reid"
  status:   stopped
  service:  not installed
  config:   /home/alice/.coop/coop.toml
  hint:     run `coop gateway install` for auto-start
```

Exit codes for automation:
- `0` = running + healthy socket handshake
- `1` = stopped or unhealthy

### Logs

If running under systemd: `journalctl --user -u coop-<agent_id>.service -n <lines>` (add `-f` if `--follow`).

But also check for a trace file — the JSONL trace is richer than the journal output. Prefer the trace file if it exists. The trace file path is known from config or the service unit's `COOP_TRACE_FILE` environment.

Implementation: read the trace file with a simple tail reader. Default mode prints the last N lines and exits. Follow mode (`-f`) keeps streaming updates. Don't pull in `libc` or anything heavy — read the last N lines by seeking from the end.

When following, handle trace rotation/truncation correctly:

- Coop rotates by renaming `traces.jsonl` and creating a fresh file at the same path
- Detect inode/metadata change or file size moving backwards
- Reopen the stable path (`traces.jsonl`) automatically

Use tokio I/O since we're already in an async context.

For human readability, format each JSONL line before printing:

```
2026-02-15 13:54:00 INFO  [agent_turn] turn complete (session=reid:main, input_tokens=5023)
2026-02-15 13:54:00 DEBUG [tool_execute] tool complete (tool=bash, output_len=142)
```

Extract `timestamp`, `level`, `span` (or first entry in `spans`), `message`, and a selection of fields. Color-code by level if stdout is a terminal. This is a simple formatter, not a full log viewer — keep it under 100 lines.

---

## Platform: launchd (macOS)

### Install

Generate `~/Library/LaunchAgents/com.coop.<agent_id>.plist` plus a wrapper script and env file:

- Wrapper script (`{launchd_wrapper}`, mode `0700`) loads env vars from `{env_file}` and execs coop.
- Env file (`{env_file}`, mode `0600`) stores `COOP_TRACE_*`, `RUST_LOG`, API keys, and custom `--env` values.

Wrapper script template:

```sh
#!/bin/sh
set -a
. "{env_file}"
set +a
exec "{binary}" start --config "{config}"
```

Plist template:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.coop.{agent_id}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{launchd_wrapper}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{stdout_log}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_log}</string>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
```

Notes:
- `RunAtLoad` + `KeepAlive/SuccessfulExit=false` means: start on login, restart if it crashes (non-zero exit), don't restart if it exits cleanly (e.g., from `coop gateway stop`).
- `ProcessType=Background` tells macOS this is a background service.
- Template plist and wrapper as string literals. No XML/shell-generation crates.
- XML-escape plist values (`&`, `<`, `>`).

After writing:

1. Resolve UID via `id -u` (`Command`), no `libc`.
2. `launchctl bootout gui/$UID/com.coop.<agent_id>` (ignore failure if not loaded)
3. If not `--no-start`: `launchctl bootstrap gui/$UID <plist_path>`
4. If not `--no-start`: `launchctl kickstart -k gui/$UID/com.coop.<agent_id>`
5. Check if auto-login is configured. LaunchAgents start at login, not at boot — so without auto-login, the gateway won't come back after a power outage until the user logs in:

   ```bash
   defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null
   ```

   If this returns empty or errors, print a note:
   ```
   note: macOS auto-login is not enabled. After a restart, the gateway
         will start when you log in (not at boot).
         To start at boot, enable auto-login in System Settings → Users & Groups.
   ```

   This is informational, not a warning — most Mac users log in quickly after a reboot, and FileVault (enabled by default) prevents auto-login regardless. Don't try to detect FileVault (requires root).

6. Print success.

### Uninstall

1. `launchctl bootout gui/$UID/com.coop.<agent_id>` (stops + unloads; ignore failure if already unloaded)
2. Remove the plist file
3. Remove `{launchd_wrapper}`
4. Remove `{env_file}`
5. Print confirmation

### Start / Stop / Restart

If plist exists:
- start:
  1. `launchctl bootstrap gui/$UID <plist_path>` (ignore "already loaded")
  2. `launchctl kickstart -k gui/$UID/com.coop.<agent_id>`
- stop: `launchctl bootout gui/$UID/com.coop.<agent_id>`
- restart:
  1. `launchctl bootout gui/$UID/com.coop.<agent_id>` (ignore failure)
  2. `launchctl bootstrap gui/$UID <plist_path>`
  3. `launchctl kickstart -k gui/$UID/com.coop.<agent_id>`

If not installed: same fallback as Linux (background spawn / PID file).

### Status

Same output format as Linux. Gather from:
- PID file + socket probe (same as Linux)
- `launchctl print gui/$UID/com.coop.<agent_id>` — parse output for PID and state
- Uptime: from PID file timestamp or `/proc` equivalent (macOS: `ps -o etime= -p <pid>`)

### Logs

launchd doesn't have journald, so logs come from the stdout/stderr files and the trace file. Prefer the trace file (JSONL, richer). Fall back to stdout log. Same tail-follow implementation as Linux.

---

## Unsupported platforms (OpenRC, runit, etc.)

On Linux without systemd (Alpine, Void, Artix, Gentoo, etc.) and other Unixes, the platform detects as `Unsupported`. All commands work except `install`/`uninstall`:

| Command | Behavior |
|---|---|
| `gateway start` | Background spawn via fallback (works everywhere) |
| `gateway stop` | PID file + SIGTERM (works everywhere) |
| `gateway restart` | stop + start (works everywhere) |
| `gateway rollback` | restore backup config (+ optional restart) |
| `gateway status` | PID file + socket probe (works everywhere) |
| `gateway logs` | Trace file tail (works everywhere) |
| `gateway install` | Refuses unless `--print` is passed |
| `gateway uninstall` | Error: no service installed |

### `gateway install --print`

When the platform is unsupported (or whenever `--print` is passed on any platform), don't write any files. Instead, print everything needed to create a service manually:

```
🐔 coop gateway — service configuration for agent "reid"

  No supported service manager detected.
  Below is the information needed to create a service manually.

  binary:      /usr/local/bin/coop
  arguments:   start --config /home/alice/.coop/coop.toml
  environment:
    COOP_TRACE_FILE=/home/alice/.coop/traces.jsonl
    ANTHROPIC_API_KEY=<captured>

  # OpenRC (/etc/init.d/coop-reid):
    command="/usr/local/bin/coop"
    command_args="start --config /home/alice/.coop/coop.toml"
    command_background=true
    pidfile="/run/coop-reid.pid"

  # runit (/etc/sv/coop-reid/run):
    #!/bin/sh
    exec chpst -u alice /usr/local/bin/coop start --config /home/alice/.coop/coop.toml

  # crontab (minimal, no crash recovery):
    @reboot COOP_TRACE_FILE=/home/alice/.coop/traces.jsonl /usr/local/bin/coop start --config /home/alice/.coop/coop.toml
```

These are hints, not full working configs — each init system has its own conventions. The goal is to give the user the resolved binary path, arguments, and environment so they don't have to reverse-engineer it.

When `--print` is used on a supported platform, print all generated artifacts to stdout (systemd unit + env file preview, or launchd plist + wrapper + env preview) instead of installing them. Redact secret values in printed env previews by default (`KEY=<redacted>`), with an explicit `--print-secrets` escape hatch only if absolutely needed.

---

## Fallback: direct background spawn

When the service manager is not installed (user ran `gateway start` without `gateway install`), or the platform is unsupported, spawn the daemon as a detached background process:

```rust
use std::process::Command;

fn spawn_background(
    binary: &Path,
    config: &Path,
    env_vars: &std::collections::BTreeMap<String, String>,
) -> Result<u32> {
    let mut cmd = Command::new(binary);
    cmd.args(["start", "--config", &config.to_string_lossy()]);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn gateway process")?;

    Ok(child.id())
}
```

For fallback start, resolve `env_vars` from the persisted service env file when present; otherwise use defaults + current process env.

After spawning, wait for the socket to appear (poll every 100ms, timeout after 5s). Verify with a Hello handshake.

For `gateway stop` without a service: read the PID file and send SIGTERM:

```rust
#[cfg(unix)]
fn kill_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()?;
    if !status.success() {
        bail!("failed to send SIGTERM to pid {pid}");
    }
    Ok(())
}
```

Don't add `nix` or `libc` as dependencies just for this. Shelling out to `kill`, `systemctl`, `launchctl`, etc. is fine — these are short-lived CLI commands, not hot paths. Use `std::process::Command` throughout.

---

## Executing system commands

Create a small helper for running external commands and capturing output:

```rust
fn run_cmd(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Run a command, returning Ok even on non-zero exit (for idempotent operations).
fn run_cmd_ignore_failure(program: &str, args: &[&str]) -> Result<()> { ... }
```

---

## Config check integration

`gateway install` must validate the config before writing the service file. Run the same checks as `coop check`. If there are errors, abort and print them. Warnings are printed but don't block installation.

Update `config_check::validate_config` with a new check:

- **`binary_exists`** (Warning): Verify `std::env::current_exe()` resolves and the binary exists. Warn if it's in a temporary or build directory (contains `target/debug` or `target/release`) — the service will break if the binary moves.

---

## Output style

All `gateway` subcommands print to stdout with a consistent style. Use emoji sparingly (just the chicken 🐔 on the header line). Use indentation for details. Print actionable hints when something is wrong.

Success:
```
🐔 coop gateway installed
  service:  coop-reid.service (systemd user)
  config:   /home/alice/.coop/coop.toml
  traces:   /home/alice/.coop/traces.jsonl
  start:    coop gateway start
```

Error:
```
error: gateway is not running
  hint: start it with `coop gateway start` or `coop gateway install` for auto-start
```

---

## What NOT to change

- **`coop start` stays as the foreground daemon.** The service manager calls `coop start --config <path>`. Don't daemonize inside `coop start` itself — let systemd/launchd handle process supervision.
- **Keep services user-scoped, not root/system-scoped.** Use systemd `--user` and launchd LaunchAgents.
- **`coop chat` stays as the embedded gateway.** It's the "just works locally" mode for development.
- **`coop attach` stays unchanged.** It connects to whatever daemon is running.
- **Don't add new crates.** Everything goes in `coop-gateway` as a `service.rs` module (and possibly `service/systemd.rs`, `service/launchd.rs` if the file gets large).
- **Don't add heavy dependencies.** No `libc`, `nix`, `service-manager`, or XML libraries. Use `std::process::Command` for system calls. Template service files as string literals.

## Tracing requirements

All new gateway lifecycle commands must emit tracing spans/events (AGENTS requirement):

- Span names: `gateway_install`, `gateway_uninstall`, `gateway_start`, `gateway_stop`, `gateway_restart`, `gateway_rollback`, `gateway_status`, `gateway_logs`
- Include fields: `agent_id`, `platform`, `service_name`, `config_path`, and command-specific fields (`follow`, `lines`, `backup_path`, etc.)
- `info!` for major state transitions, `warn!` for recoverable fallbacks, `error!` for failed external commands
- Record external command executions (`program`, `args`, exit status, stderr snippet)
- Never log secret env values. Redact by key name in both console and trace events.

After implementation, verify with `COOP_TRACE_FILE=traces.jsonl` that these events appear with expected fields.

## Testing

### Unit tests in `service.rs`

- Service name sanitization (agent ID → service name)
- PID file read/write/cleanup
- `is_pid_alive` with current PID (should return true) and PID 0 or MAX (should return false)
- Systemd unit generation (`EnvironmentFile=` path, no `%i`)
- Launchd plist generation (wrapper-based `ProgramArguments`)
- XML escaping of values containing `&`, `<`, `>`
- Env file generation and permissions (0600)
- Environment variable capture logic (filters correctly, doesn't capture HOME/PATH)
- `--print` redaction behavior (`print_secrets=false` redacts)
- Rollback backup path resolution + failed-config snapshot naming
- Log tail follower rotation detection (inode/size rollback -> reopen)

### Integration tests

These are harder to automate (require systemd/launchd) and are best verified manually:

1. `coop gateway install` → service file exists, env file exists with 0600 perms, service enabled
2. `coop gateway start` → process running, socket accepting connections
3. `coop gateway status` → shows running with correct PID
4. `coop gateway stop` → process stopped, socket gone
5. `coop gateway logs` (no `-f`) → prints last N lines and exits
6. `coop gateway logs -f` → streams appended lines and reopens after trace rotation
7. `coop gateway rollback` with valid backup → config restored and gateway healthy after restart
8. `coop gateway uninstall` → service metadata removed and disabled/unloaded
9. `coop gateway start` (without install) → spawns background process via fallback
10. Crash recovery: `kill -9 <pid>` → supervisor restarts within 5s (systemd/launchd)

## Development sequence

1. **PID file support in `cmd_start`.** Write/read/cleanup. This is useful immediately regardless of service management.
2. **CLI structure.** Add `GatewayCommands` to clap, wire up `cmd_gateway` dispatch in `main.rs`.
3. **Platform detection + service paths.** `detect_platform`, `ServicePaths`, name sanitization, env/wrapper file paths.
4. **`gateway status`.** Implement first — it's read-only and exercises PID file + socket probe logic.
5. **`gateway install` + `gateway uninstall`.** Unit/plist + env/wrapper generation, secret handling, enable/start behavior.
6. **`gateway start` + `gateway stop` + `gateway restart`.** Service manager delegation + fallback.
7. **`gateway rollback`.** Backup resolution, validation, atomic restore, failed-config snapshot, optional restart + health check.
8. **`gateway logs`.** Last-N default, `-f` follow mode, rotation-aware reopen, JSONL formatter.
9. **Config check integration.** Add `binary_exists` check.
10. **Tracing instrumentation.** Add spans/events for every gateway command and verify in traces.
11. **Manual end-to-end verification.** Walk through the integration test scenarios above.
