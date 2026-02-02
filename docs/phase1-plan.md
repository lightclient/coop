# Phase 1: Gateway + Terminal TUI

## Goal
A running Coop gateway daemon that accepts messages from a terminal TUI, routes them to a Goose agent session, and streams responses back. No channels, no multi-user, no persistence yet — just the core loop proving the architecture works.

## What We're Building

```
┌──────────────┐         ┌──────────────────────────┐
│  Terminal TUI │ ←stdin→ │     Coop Gateway          │
│  (ratatui)    │         │                            │
│               │         │  Config → Agent Pool       │
│  input bar    │         │           ↓                │
│  message list │         │     Goose Runtime          │
│  status bar   │         │      (tool loop)           │
│               │         │           ↓                │
└──────────────┘         │     Tool Results           │
                          └──────────────────────────┘
```

## Milestones

### M1: Skeleton Binary
- [ ] `cargo init` with workspace layout
- [ ] CLI with `clap`: `coop start`, `coop chat`
- [ ] Config loading from `coop.yaml` (serde + basic validation)
- [ ] Tokio runtime boots, logs "gateway started", shuts down on ctrl-c
- [ ] Structured logging with `tracing` (stdout + optional file)

### M2: Goose Integration
- [ ] Investigate Goose as a library vs. fork
  - Can we import `goose-core` as a crate?
  - Or do we shell out to `goose` CLI via stdio JSON?
  - Or fork and extract the session/tool loop?
- [ ] Wrap Goose in an `AgentRuntime` trait so we can swap later
- [ ] Send a hardcoded prompt, get a response, print it
- [ ] Verify tool calling works (e.g. agent calls a simple built-in tool)

### M3: Session Manager
- [ ] `SessionManager` struct — creates/retrieves sessions by key
- [ ] In-memory conversation history (Vec<Message>)
- [ ] Session key: `(agent_id, user_id, kind)` 
- [ ] Pass conversation history to Goose on each turn
- [ ] Handle streaming responses (token-by-token callback)

### M4: Terminal TUI
- [ ] `ratatui` based terminal UI
- [ ] Layout: message history (scrollable), input bar, status line
- [ ] Input: type message, press Enter to send
- [ ] Streaming: tokens appear as they arrive from the agent
- [ ] Status bar: model name, token count, session info
- [ ] Ctrl-C or `/quit` to exit
- [ ] `/clear` to reset session
- [ ] Markdown rendering in terminal (basic: bold, code blocks, lists)

### M5: Basic Config + Trust
- [ ] Parse `coop.yaml` with agent config (model, personality file, instructions file)
- [ ] Single user (owner) with `trust: full`
- [ ] Load personality + instructions into system prompt
- [ ] Memory store paths defined in config (read-only for now)
- [ ] Trust level plumbed through but only `full` implemented

### M6: File Tools
- [ ] Built-in `read` tool — read file contents
- [ ] Built-in `write` tool — write file contents
- [ ] Built-in `edit` tool — find/replace in files
- [ ] Tool execution gated by trust level (all allowed at `full`)
- [ ] Agent can read/write its own workspace files

## Non-Goals for Phase 1
- No Signal/Telegram/iMessage channels (terminal only)
- No SQLite persistence (in-memory sessions)
- No multi-agent (single agent)
- No multi-user (single owner)
- No cron/heartbeat scheduling
- No vector search / embeddings
- No sub-agent spawning

## Directory Structure

```
coop/
├── Cargo.toml              # workspace root
├── coop.yaml               # default config location
├── crates/
│   ├── coop-gateway/       # the daemon
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs     # entry point, CLI
│   │       ├── gateway.rs  # core event loop
│   │       ├── config.rs   # config parsing
│   │       ├── session.rs  # session manager
│   │       ├── router.rs   # message routing (trivial for phase 1)
│   │       └── trust.rs    # trust resolution
│   │
│   ├── coop-agent/         # agent runtime abstraction
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── runtime.rs  # AgentRuntime trait
│   │       ├── goose.rs    # Goose implementation
│   │       └── tools/
│   │           ├── mod.rs
│   │           ├── read.rs
│   │           ├── write.rs
│   │           └── edit.rs
│   │
│   └── coop-tui/           # terminal interface
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── app.rs      # app state
│           ├── ui.rs       # ratatui layout/rendering
│           └── input.rs    # key handling
│
├── docs/
│   ├── architecture.md
│   ├── design.md
│   └── phase1-plan.md
│
└── workspaces/
    └── default/            # default workspace for phase 1
        ├── soul.md
        └── agents.md
```

## Key Decisions to Make First

### 1. Goose Integration Strategy
This is the critical unknown. Three options:

**A) Goose as a library crate**
Import Goose's core session/tool loop directly. Best performance, tightest integration. Depends on whether Goose's code is structured for this — it may be tightly coupled to its own CLI.

**B) Goose as a subprocess**
Shell out to `goose` CLI, communicate via stdin/stdout JSON. Simplest to start, loosest coupling. Adds latency and complexity for streaming. Similar to how OpenClaw wraps Claude Code.

**C) Fork Goose, extract core**
Fork the repo, pull out the session management and tool-calling loop into a standalone crate. Most work upfront, most control long-term.

**Recommendation:** Start with B (subprocess) to unblock everything else. Investigate A in parallel. Move to A or C once we understand Goose's internals.

### 2. TUI Framework
`ratatui` is the standard. Alternatives: `cursive`, `tui-rs` (deprecated, ratatui is the successor). Go with `ratatui`.

### 3. Config Format
YAML with `serde_yaml`. Considered TOML but YAML handles nested structures (like the trust levels and user lists) more naturally.

## Dependencies (Phase 1)

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
ratatui = "0.29"
crossterm = "0.28"
anyhow = "1"
```

## Success Criteria
- `coop start` launches the gateway daemon
- `coop chat` opens a terminal TUI connected to the gateway
- Type a message, get a streamed response from Claude/GPT via Goose
- Agent can read/write files in its workspace
- Ctrl-C shuts down cleanly
- Config loads from `coop.yaml`
