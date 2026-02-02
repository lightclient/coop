# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Coop is a personal agent gateway in Rust that routes messages between channels (Signal, Telegram, Discord, terminal TUI, webhooks) and AI agent sessions. It manages trust-based access control, persists conversations, and handles agent lifecycles. Currently in Phase 1 (gateway + terminal TUI).

## Commands

```bash
just check              # Run full CI: fmt, toml, lint, deny, test
just fmt                # Format (cargo fmt + taplo fmt)
just lint               # cargo clippy --all-targets --all-features -- -D warnings
just test               # cargo test --all
just build              # cargo build --release
just run                # cargo run --bin coop (TUI)
just fix                # Auto-fix formatting + clippy issues

cargo test -p coop-core       # Test a single crate
cargo test -p coop-core -- prompt  # Run tests matching "prompt"
```

## Architecture

Five workspace crates under `crates/`:

- **coop-core** — Domain types (`Message`, `Role`, `Content`, `SessionKey`, `TrustLevel`), trait boundaries (`Provider`, `Channel`, `Tool`, `ToolExecutor`, `SessionStore`), prompt builder with token counting, and testing fakes for all traits
- **coop-agent** — LLM provider integration: direct Anthropic API client with OAuth support, Goose runtime wrapper, and a conversion layer (`convert.rs`) bridging Coop ↔ Goose types at the boundary
- **coop-gateway** — Main binary entry point, CLI (Start/Chat/Version), TUI event loop, gateway message routing, YAML config parsing
- **coop-channels** — Channel adapters (currently terminal only)
- **coop-tui** — Terminal UI built on ratatui/crossterm

**Entry points:**
- CLI/TUI: `crates/coop-gateway/src/main.rs`
- Gateway: `crates/coop-gateway/src/gateway.rs`
- Traits: `crates/coop-core/src/traits.rs`
- Types: `crates/coop-core/src/types.rs`
- Test fakes: `crates/coop-core/src/fakes.rs`

**Key design patterns:**
- All external integrations are behind traits in `coop-core/traits.rs` — providers, channels, tools, session stores
- Matching fake implementations in `coop-core/fakes.rs` (`FakeProvider`, `FakeChannel`, `FakeTool`, `MemorySessionStore`) for testing without real dependencies
- Goose is an implementation detail wrapped behind the `Provider` trait; Coop types are the source of truth
- System prompts assembled via layered `PromptBuilder` with token budgeting and Anthropic cache hints
- Trust model uses Bell-LaPadula ordering: `Full < Inner < Familiar < Public`
- Config loaded from YAML (`coop.yaml`) with hierarchical path resolution

## Code Conventions

- Error handling: `anyhow::Result` everywhere
- `unsafe` code is denied at workspace level
- Clippy: `all` + `pedantic` warnings enabled (with targeted allows in `Cargo.toml`)
- rustfmt: edition 2024, max_width=100, field init shorthand
- TOML files formatted with `taplo`
- Logging via `tracing` — errors and key state transitions only
- Comments only for "why", not "what"
- Never commit PII — use Alice/Bob/Carol placeholders and `test-token` credentials
- Don't edit `Cargo.toml` dependency versions manually when `cargo add` works
