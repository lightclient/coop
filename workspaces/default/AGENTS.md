# Instructions

You are an AI agent. Help the user with their tasks.
When using tools, explain what you're doing briefly.

## Heartbeat Protocol

Cron heartbeat messages ask you to check HEARTBEAT.md for pending tasks. Your response will be delivered to the user's channels (Signal, etc.).

- If nothing needs attention, reply with exactly **HEARTBEAT_OK** — this suppresses delivery so the user isn't bothered.
- If there is something to report, reply with the actual content. Do NOT include HEARTBEAT_OK alongside real content.
- Keep heartbeat responses concise — these are push notifications, not conversations.

# Project

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
- **coop-agent** — LLM provider integration: direct Anthropic API client with OAuth support
- **coop-gateway** — Main binary entry point, CLI (Start/Chat/Version), TUI event loop, gateway message routing, YAML config parsing
- **coop-channels** — Channel adapters (currently terminal only)
- **coop-tui** — Terminal UI built on crossterm

**Entry points:**
- CLI/TUI: `crates/coop-gateway/src/main.rs`
- Gateway: `crates/coop-gateway/src/gateway.rs`
- Traits: `crates/coop-core/src/traits.rs`
- Types: `crates/coop-core/src/types.rs`
- Test fakes: `crates/coop-core/src/fakes.rs`

**Key design patterns:**
- All external integrations are behind traits in `coop-core/traits.rs` — providers, channels, tools, session stores
- Matching fake implementations in `coop-core/fakes.rs` (`FakeProvider`, `FakeChannel`, `FakeTool`, `MemorySessionStore`) for testing without real dependencies
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
