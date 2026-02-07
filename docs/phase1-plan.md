# Phase 1: Gateway + Terminal TUI

## Goal
A running Coop gateway daemon that accepts messages from a terminal TUI, routes them to an agent session, and streams responses back. No channels, no multi-user, no persistence yet — just the core loop proving the architecture works.

## What We're Building

```
┌──────────────┐         ┌──────────────────────────┐
│  Terminal TUI │ ←stdin→ │     Coop Gateway          │
│  (ratatui)    │         │                            │
│               │         │  Config → Agent Pool       │
│  input bar    │         │           ↓                │
│  message list │         │     Agent Runtime           │
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

### M2: Provider Integration
- [x] Direct Anthropic API client with streaming support
- [x] OAuth token support (Claude Code subscriptions)
- [x] Wrap behind `Provider` trait so we can swap later
- [x] Send a hardcoded prompt, get a response, print it
- [x] Verify tool calling works (e.g. agent calls a simple built-in tool)

### M3: Session Manager
- [ ] `SessionManager` struct — creates/retrieves sessions by key
- [ ] In-memory conversation history (Vec<Message>)
- [ ] Session key: `(agent_id, user_id, kind)` 
- [ ] Pass conversation history to provider on each turn
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
│   ├── coop-gateway/       # the daemon — owns the event loop, routing, CLI
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs     # entry point, CLI
│   │       ├── gateway.rs  # core event loop
│   │       ├── config.rs   # config parsing + validation
│   │       ├── router.rs   # message routing (pure logic)
│   │       └── trust.rs    # trust resolution (pure logic)
│   │
│   ├── coop-core/          # shared types and trait definitions
│   │   ├── Cargo.toml      # NO external dependencies beyond serde
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── types.rs    # Message, SessionKey, TrustLevel, etc.
│   │       ├── channel.rs  # Channel trait
│   │       ├── runtime.rs  # AgentRuntime trait
│   │       ├── tools.rs    # Tool trait + ToolContext
│   │       ├── memory.rs   # MemoryIndex trait
│   │       ├── session.rs  # SessionStore trait
│   │       └── fakes.rs    # FakeChannel, FakeRuntime, FakeTool, etc.
│   │
│   ├── coop-agent/         # agent runtime — Anthropic provider
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── anthropic_provider.rs  # Anthropic API client
│   │       └── tools/
│   │           ├── mod.rs
│   │           ├── read.rs
│   │           ├── write.rs
│   │           └── edit.rs
│   │
│   ├── coop-channels/      # channel implementations
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── terminal.rs # terminal/TUI channel (phase 1)
│   │       └── signal/     # (phase 2)
│   │           ├── mod.rs
│   │           └── fixtures/
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
│   ├── phase1-plan.md
│   └── testing-strategy.md
│
└── workspaces/
    └── default/            # default workspace for phase 1
        ├── soul.md
        └── agents.md
```

### Crate Dependency Graph

```
coop-core (traits, types, fakes — zero external deps)
    ↑            ↑             ↑
coop-agent   coop-channels   coop-gateway
(anthropic)  (signal, etc.)  (router, config)
    ↑            ↑             ↑
    └────────────┴─────────────┘
                 ↑
             coop-tui
```

Key rule: **coop-core has no external dependencies** beyond serde. It defines the contracts. Everything else depends on it. Tests live close to the logic they test — integration tests in coop-gateway test the full flow using fakes from coop-core.

## Key Decisions Made

### 1. Provider Strategy
**Decided:** Build our own direct Anthropic API client with OAuth support. No external agent runtime dependency. Coop owns the full tool-calling loop, session management, and streaming.

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
- Type a message, get a streamed response from Claude via Anthropic API
- Agent can read/write files in its workspace
- Ctrl-C shuts down cleanly
- Config loads from `coop.yaml`
