```
    ████████
  ██▓▓▓▓▓▓▓▓██
  ██▓▓▓▓▓▓▓▓██
  ██▓▓██▓▓██▓▓██
  ██▓▓▓▓▓▓▓▓██
    ████████
```

# Coop

A personal agent gateway in Rust. Coop routes messages between channels (Signal, Telegram, Discord, terminal, webhooks) and AI agent sessions running on your machine. It enforces trust-based access control, persists conversations, and manages agent lifecycles.

**Status:** Phase 1 — gateway + terminal TUI.

## Quick start

```bash
cp coop.example.yaml coop.yaml   # configure your agent + provider
just run                          # launch the TUI
```

## Architecture

Five workspace crates:

| Crate | Purpose |
|-------|---------|
| `coop-core` | Domain types, trait boundaries, prompt builder, test fakes |
| `coop-agent` | LLM provider integration (Anthropic API, Goose runtime) |
| `coop-gateway` | CLI entry point, TUI event loop, gateway routing, config |
| `coop-channels` | Channel adapters (terminal; Signal/Telegram/Discord planned) |
| `coop-tui` | Terminal UI (ratatui/crossterm) |

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
