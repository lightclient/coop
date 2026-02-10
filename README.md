```
        ████████████████
        ████████████████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓████▓▓▓▓████▓▓▓▓████
    ████▓▓▓▓████▓▓▓▓████▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
    ████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓████
        ████████████████
        ████████████████
```

[![CI](https://github.com/lightclient/coop/actions/workflows/ci.yml/badge.svg)](https://github.com/lightclient/coop/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/lightclient/coop)](https://github.com/lightclient/coop/releases)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](LICENSE-MIT)

# Coop

**Coop** is a personal agent gateway, focused on multi-user access of an
evolving agent. Unlike other platforms, Coop has a native concept of "trust
levels" that control what data, memories, etc are shared in various contexts.
This is managed by the gateway, not via flakey prompt coercion.

## Focus

- **Native permissions for data**: sessions only have access to what they're
  given
- **Self-configuration**: the agent must be *exceptional* at configuring itself.
  Feature awareness, hot configuration swapping, configuration validation, etc.
  Coop is extensible by just chatting. 
- **Shared experience**: Coop is designed to be shared with partners, families,
  friends, etc. The native permissions for data allows for varying levels of
  trust depending on context.
- **trace driven debugging**: agents are great at navigating traces. We
  accelerate development in Coop by integrating thorough local telemetry so that
  traces can always be compared with code to isolate and fix issues.

## Install

```bash
cargo install --git https://github.com/lightclient/coop coop-gateway
```

This builds and installs the `coop` binary to `~/.cargo/bin/`.

## Quick start

```bash
coop init

# Set your API key (standard Anthropic key or Claude Code OAuth token)
export ANTHROPIC_API_KEY="sk-ant-..."

coop start

# In another terminal, attach a TUI session
coop attach
```

## Configuration

Coop is configured via a single `coop.toml` file. Below is a complete example
showing every available option with comments. Only `[agent]` is required —
everything else has sensible defaults.

```toml
# ---------------------------------------------------------------------------
# Agent — the core identity (required)
# ---------------------------------------------------------------------------
[agent]
id = "cooper"                        # Agent name
model = "anthropic/claude-opus-4-6"  # Model to use
workspace = "./workspaces/default"   # Path to workspace directory


# ---------------------------------------------------------------------------
# Users — who can talk to the agent and what they're allowed to do
# ---------------------------------------------------------------------------
# Trust levels control tool access and memory visibility:
#   full     — all tools, all memory stores, all workspace files, config, etc
#   inner    — bash/file tools, shared + social memory
#   familiar — read-only tools, social memory only
#   public   — no tools, no memory
#
# The "match" field binds a user to channels. The format is "channel:identifier".
# A user can be matched to multiple channels.

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]

[[users]]
name = "bob"
trust = "inner"
match = ["signal:bob-uuid"]


# ---------------------------------------------------------------------------
# Provider — LLM backend (currently only "anthropic")
# ---------------------------------------------------------------------------
[provider]
name = "anthropic"

# Optional: multiple API keys for automatic rotation on rate limits.
# Each entry references an environment variable with the "env:" prefix.
# Keys rotate proactively at 90% utilization and reactively on 429 errors.
# When omitted, falls back to the ANTHROPIC_API_KEY environment variable.
#
# api_keys = ["env:ANTHROPIC_API_KEY", "env:ANTHROPIC_API_KEY_2"]


# ---------------------------------------------------------------------------
# Channels — where messages come from
# ---------------------------------------------------------------------------
# Signal (requires linking — see "Signal setup" below)
# [channels.signal]
# db_path = "./db/signal.db"    # Path to signal-cli database
# verbose = false               # Send partial replies on each tool-call boundary


# ---------------------------------------------------------------------------
# Memory — structured observation store backed by SQLite
# ---------------------------------------------------------------------------
# Memory is enabled by default with no config needed. The agent gets tools to
# search, write, and browse observations across trust-gated stores:
#   private — full trust only (personal facts, secrets)
#   shared  — full + inner trust (project context, decisions)
#   social  — full + inner + familiar trust (public-safe info)

[memory]
db_path = "./db/memory.db"                     # SQLite database path

# Prompt index — injects recent relevant memories into the system prompt
# so the agent has context without explicitly searching.
[memory.prompt_index]
enabled = true        # Toggle the prompt index (default: true)
limit = 12            # Max observations to include (default: 12)
max_tokens = 1200     # Token budget for the index block (default: 1200)

# Retention — automatic compression, archiving, and cleanup of old observations.
# Runs at startup and periodically in the background.
[memory.retention]
enabled = true                     # Toggle retention maintenance (default: true)
compress_after_days = 14           # Cluster and merge stale observations (default: 14)
compression_min_cluster_size = 3   # Min cluster size to trigger compression (default: 3)
archive_after_days = 30            # Move expired observations to archive (default: 30)
delete_archive_after_days = 365    # Permanently delete old archives (default: 365)
max_rows_per_run = 200             # Max rows processed per maintenance run (default: 200)

# Embedding — optional semantic vector search. Without this, search is
# full-text only (FTS5), which works well for most use cases.
# Supported providers: openai, voyage, cohere, openai-compatible
#
# [memory.embedding]
# provider = "openai"                # Provider name
# model = "text-embedding-3-small"   # Embedding model
# dimensions = 1536                  # Vector dimensions
#
# For openai-compatible endpoints:
# base_url = "https://your-endpoint/v1"
# api_key_env = "YOUR_API_KEY_ENV_VAR"


# ---------------------------------------------------------------------------
# Prompt files — control which files are included in the system prompt
# ---------------------------------------------------------------------------
# Shared files are loaded once per session from the workspace root.
# User files are loaded per-user from workspaces/<workspace>/users/<name>/.
#
# Each file has a trust gate (minimum trust level to see it) and a cache hint:
#   stable   — rarely changes (e.g. personality)
#   session  — changes between sessions
#   volatile — changes within a session
#
# The defaults below work out of the box. Override to add custom files or
# change visibility.

# [[prompt.shared_files]]
# path = "SOUL.md"                   # Agent personality and voice
# trust = "familiar"                 # Visible to familiar+ users
# cache = "stable"
#
# [[prompt.shared_files]]
# path = "IDENTITY.md"
# trust = "familiar"
# cache = "session"
#
# [[prompt.shared_files]]
# path = "TOOLS.md"
# trust = "full"
# cache = "session"
#
# [[prompt.shared_files]]
# path = "BOOTSTRAP.md"             # First-run bootstrap instructions
# trust = "full"
# cache = "volatile"
#
# [[prompt.user_files]]
# path = "AGENTS.md"                # Behavioral instructions
# trust = "full"
# cache = "stable"
#
# [[prompt.user_files]]
# path = "USER.md"                  # Per-user info
# trust = "inner"
# cache = "session"
#
# [[prompt.user_files]]
# path = "TOOLS.md"                 # Per-user tool notes
# trust = "full"
# cache = "session"


# ---------------------------------------------------------------------------
# Cron — scheduled tasks
# ---------------------------------------------------------------------------
# The scheduler runs inside the daemon and sends messages to the agent on a
# cron schedule. When a cron entry has a "user" field, the response is delivered
# to all non-terminal channels that user is bound to (from their match patterns).
# An explicit "deliver" field overrides this with a specific target.
#
# If the agent responds with only "HEARTBEAT_OK", delivery is suppressed
# (nothing to report). An empty HEARTBEAT.md skips the LLM call entirely.

# [[cron]]
# name = "heartbeat"
# cron = "*/30 * * * *"              # Standard cron expression
# user = "alice"                     # Run as this user (optional)
# message = "check HEARTBEAT.md"     # Message sent to the agent
#
# [[cron]]
# name = "morning-briefing"
# cron = "0 8 * * *"
# user = "alice"
# message = "Morning briefing"
# [cron.deliver]                     # Explicit delivery target
# channel = "signal"
# target = "alice-uuid"              # Or "group:<hex>" for group chats
#
# [[cron]]
# name = "cleanup"                   # No user, no delivery — silent internal work
# cron = "0 3 * * *"
# message = "run cleanup"
```

### Signal setup

Link coop to your Signal account as a secondary device:

```bash
coop signal link
```

This displays a QR code — scan it with Signal on your phone (Settings → Linked Devices → Link New Device).

To find your Signal UUID for the `match` field, send a message to the linked device and check the trace output:

```bash
COOP_TRACE_FILE=traces.jsonl coop start
# Send a message from your phone, then:
grep '"signal"' traces.jsonl | head -5
```

The sender UUID in the trace is what goes in your config:

```toml
[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:<your-uuid>"]
```

### Hot reload

The config file is watched for changes. These fields take effect immediately without a restart:

- `agent.model`, `users`, `cron`, `memory.prompt_index`, `memory.retention`

These require a restart: `agent.id`, `agent.workspace`, `provider`, `channels`, `memory.db_path`, `memory.embedding`

## Workspace

Agent personality and context live in markdown files in the workspace directory. All files are optional.

| File | Purpose |
|------|---------|
| `SOUL.md` | Agent personality and voice |
| `IDENTITY.md` | Agent identity |
| `AGENTS.md` | Behavioral instructions |
| `TOOLS.md` | Tool usage notes |
| `USER.md` | Per-user info |
| `HEARTBEAT.md` | Periodic check tasks |

Channel-specific formatting instructions can be added as `channels/<name>.md` (e.g. `channels/signal.md`).

## Architecture

```
crates/
├── coop-core       # Shared types, traits, prompt builder, test fakes
├── coop-agent      # LLM provider integration (Anthropic API)
├── coop-memory     # Structured memory store (SQLite + FTS5)
├── coop-gateway    # CLI entry point, daemon, gateway routing, config
├── coop-ipc        # Unix socket IPC protocol
├── coop-channels   # Channel adapters (terminal, Signal)
└── coop-tui        # Terminal UI (crossterm)
```

## Development

```bash
cargo build                  # Build
cargo test                   # Run all tests
just check                   # Full CI: fmt, lint, deny, test
just fix                     # Auto-fix formatting + clippy
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
