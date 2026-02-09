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

# Coop

A personal agent gateway in Rust. Coop routes messages between channels (Signal, Telegram, Discord, terminal, webhooks) and AI agent sessions running on your machine. It enforces trust-based access control, persists conversations, and manages agent lifecycles.

- ~~Phase 1 — gateway + terminal TUI.~~
- ~~Phase 2 - separate gateway + telemetry = dogfooding, tight LLM loop~~
- **Phase 3 - chat channel integration**
- Phase 4 - user and permissions

## Quick start

Coop needs `ANTHROPIC_API_KEY` in your environment. You can use a standard API key from [console.anthropic.com](https://console.anthropic.com/), or reuse your Claude Code OAuth token:

```bash
export ANTHROPIC_API_KEY=$(jq -r '.claudeAiOauth.accessToken' ~/.claude/.credentials.json)
```

```bash
# terminal 1
cargo run --bin coop -- start

# terminal 2
cargo run --bin coop -- chat
```

## Architecture

Five workspace crates:

| Crate | Purpose |
|-------|---------|
| `coop-core` | Domain types, trait boundaries, prompt builder, test fakes |
| `coop-agent` | LLM provider integration (Anthropic API) |
| `coop-gateway` | CLI entry point, daemon lifecycle, gateway routing, config |
| `coop-ipc` | Unix socket IPC protocol and client/server transport |
| `coop-channels` | Channel adapters (terminal; Signal scaffolded) |
| `coop-tui` | Terminal UI (crossterm) |

## Workspace

Agent personality and context live in workspace files (default: `./workspaces/default/`):

| File | Purpose | Trust |
|------|---------|-------|
| `SOUL.md` | Agent personality and voice | familiar |
| `AGENTS.md` | Behavioral instructions | familiar |
| `TOOLS.md` | Tool usage notes | familiar |
| `IDENTITY.md` | Agent identity | familiar |
| `USER.md` | Per-user info | inner |
| `MEMORY.md` | Long-term curated memory | full |
| `HEARTBEAT.md` | Periodic check tasks | full |

All files are optional. Trust level controls which files are visible in a given session — see [System Prompt Design](docs/system-prompt-design.md).

### Channel prompts

Coop injects channel-specific formatting instructions into the system prompt so the agent adapts its output to each channel's capabilities. For example, Signal messages use plain text (no markdown), while terminal sessions get rich formatting.

Built-in defaults are provided for known channels:

| Channel | Default behavior |
|---------|-----------------|
| `signal` | Plain text only — no markdown, asterisks, backticks, code fences, or bullet markers |
| `terminal` | No restrictions (supports rich formatting) |

To override the built-in or add instructions for a new channel, create a file in the workspace:

```
workspaces/default/channels/signal.md     # override Signal default
workspaces/default/channels/discord.md    # add Discord-specific instructions
```

The file content replaces the built-in default entirely. The channel name is the part before the first colon in the channel identifier (`terminal:default` → `terminal`, `signal` → `signal`).

## Development

```bash
just check    # fmt, toml, lint, deny, test
just fmt      # auto-format
just lint     # clippy
just test     # cargo test --all
just build    # release build
```

## Docs

- [Architecture](docs/architecture.md) — core concepts and high-level design
- [Design](docs/design.md) — full design document with config, trust model, and build phases
- [Phase 1 Plan](docs/phase1-plan.md) — gateway + terminal TUI (current milestone)
- [Testing Strategy](docs/testing-strategy.md) — trait boundaries, fakes, fixture-driven testing
- [Memory Design](docs/memory-design.md) — structured observations, SQLite + FTS5, progressive disclosure

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
