# AGENTS Instructions

Coop is a multi-agent gateway in Rust with a TUI interface. It routes messages between channels (CLI, webhooks) and AI provider backends, managing sessions and agent lifecycles.

## ⚠️ Privacy Rule — No Personal Information

**Never commit real names, phone numbers, addresses, API keys, tokens, or any personally identifiable information (PII) to this repository.** This applies everywhere: source code, tests, docs, examples, config files, comments, commit messages.

Use standard cryptography placeholder names ([Alice and Bob convention](https://en.wikipedia.org/wiki/Alice_and_Bob)):

| Name | Role |
|------|------|
| **Alice** | Primary user / owner (full trust) |
| **Bob** | Secondary user / partner (inner trust) |
| **Carol** | Third participant (familiar trust) |
| **Dave** | Fourth participant |
| **Eve** | Eavesdropper (passive attacker) |
| **Mallory** | Malicious active attacker (MITM, replay, etc.) |
| **Trent** | Trusted third party / authority |
| **Grace** | Government representative |
| **Faythe** | Trusted advisor / courier |

Other fake data:
- Emails: `alice@example.com`, `bob@example.com`
- Phones: `+15555550100`, `+15555550101`
- Addresses: `123 Test St`
- Tokens: `test-token`, `sk-test-xxx`

If you find real PII in the repo, remove it immediately and rewrite git history if needed.

## Setup
```bash
cargo build
```

## Commands

### Build
```bash
cargo build                   # debug
cargo build --release         # release
```

### Test
```bash
cargo test                    # all tests
cargo test -p coop-core       # specific crate
cargo test -p coop-gateway
```

### Lint/Format
```bash
cargo fmt
cargo clippy -- -D warnings
```

## Structure
```
crates/
├── coop-agent        # agent logic & provider interaction
├── coop-channels     # channel adapters (CLI, webhooks, etc.)
├── coop-core         # shared types, config, message routing
├── coop-gateway      # gateway server, session management
└── coop-tui          # terminal UI (ratatui)

docs/                 # design docs
workspaces/           # workspace data
```

## Development Loop
```bash
# 1. Make changes
# 2. cargo fmt
# 3. cargo build
# 4. cargo test -p <crate>
# 5. cargo clippy -- -D warnings
```

## Rules

Error: Use `anyhow::Result` for error handling
Test: Prefer `tests/` folders within each crate
Test: Use fake/placeholder data only — never real PII

## Code Quality

Comments: Write self-documenting code — prefer clear names over comments
Comments: Only comment for complex algorithms, non-obvious logic, or "why" not "what"
Simplicity: Don't over-abstract early. Trust Rust's type system.
Errors: Don't add redundant error context
Logging: Use tracing. Don't over-log — errors and key state transitions only.

## Never

Never: Commit personal information, real names, real credentials, or PII
Never: Skip `cargo fmt`
Never: Merge without `cargo clippy -- -D warnings` passing
Never: Comment self-evident operations
Never: Edit `Cargo.toml` dependency versions manually when `cargo add` works
