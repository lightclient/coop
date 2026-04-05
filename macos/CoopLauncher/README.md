# Coop Launcher

A small macOS app that embeds Coop's TUI in a PTY-backed terminal view.

This is the stable GUI entrypoint you can later sign and grant Full Disk Access
without re-signing your changing Rust binaries on every rebuild.

## What it does

- creates a real macOS app bundle
- embeds a terminal with [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm)
- launches Coop as a direct child process on a PTY
- keeps mutable Coop artifacts outside the app bundle
- writes a generated helper script to `~/Library/Application Support/CoopLauncher/run-coop.zsh`

## Build

```bash
just launcher-build
```

Or directly:

```bash
./macos/CoopLauncher/build-app.sh
```

The app bundle is written to:

```text
macos/CoopLauncher/dist/Coop Launcher.app
```

## First run

On first launch, the app creates:

```text
~/Library/Application Support/CoopLauncher/
  config.json
  run-coop.zsh
  logs/
```

If it cannot guess your repo location, click **Choose Repo…** and select the
Coop checkout root.

## Launch modes

The toolbar popup supports:

- **Cargo Run** — runs `cargo run -p coop-gateway --bin coop -- ...`
- **Debug Binary** — runs `<repo>/target/debug/coop ...`
- **Custom Executable** — runs `custom_executable_path` from `config.json`

`Cargo Run` is the default because it works before you have a built binary.

## Config file

Path:

```text
~/Library/Application Support/CoopLauncher/config.json
```

Example:

```json
{
  "repo_path": "/Users/alice/src/coop/browser",
  "launch_mode": "debugBinary",
  "arguments": ["chat"],
  "environment": {
    "RUST_LOG": "info"
  },
  "trace_file": "traces.jsonl",
  "window_title": "Coop Launcher"
}
```

For custom executable mode:

```json
{
  "repo_path": "/Users/alice/src/coop/browser",
  "launch_mode": "customExecutable",
  "custom_executable_path": "/Users/alice/bin/coop-dev.sh",
  "arguments": ["chat"],
  "environment": {
    "RUST_LOG": "info"
  },
  "trace_file": "traces.jsonl",
  "window_title": "Coop Launcher"
}
```

Do not point `custom_executable_path` at the generated
`~/Library/Application Support/CoopLauncher/run-coop.zsh` helper. That file is
exported from the launcher config for external use, not as the launch target for
Custom Executable mode.

## Notes

- The launcher is intentionally **not sandboxed**.
- The changing Coop binary stays outside the app bundle.
- The generated `run-coop.zsh` script is rewritten from `config.json`.
- This launcher is macOS-only and is not part of the Rust workspace build.
