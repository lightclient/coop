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
cargo fmt                     # also: taplo fmt for TOML files
cargo clippy --all-targets --all-features -- -D warnings
just check                    # full CI: fmt, toml, lint, deny, test
just fix                      # auto-fix formatting + clippy
```

## Structure
```
crates/
├── coop-agent        # provider integration (Anthropic)
├── coop-channels     # channel adapters (terminal, future: Signal, etc.)
├── coop-core         # shared types, traits, prompt builder, test fakes
├── coop-gateway      # gateway server, CLI entry, session management
└── coop-tui          # terminal UI (crossterm)

docs/                 # design docs
workspaces/           # agent workspace data (personality, instructions)
```

## Compile Times — Read This

Fast incremental builds are critical. Coop is developed through agentic loops where an AI agent edits, builds, tests, and iterates. Every extra second of compile time compounds across hundreds of iterations per session. See `docs/compile-times.md` for the full rationale and toolchain setup.

**Current targets:** incremental leaf build <1s, incremental root build <1.5s.

**Rules for keeping builds fast:**

1. **Split large files.** Don't let any single `.rs` file grow past ~500 lines. Large files in leaf crates (especially `coop-gateway`) defeat incremental compilation. Extract focused modules: `cli.rs`, `tui_helpers.rs`, etc. Rust's incremental compiler tracks per-function dependencies, so smaller files = less recompilation on change.

2. **Don't bloat `coop-core`.** Every crate depends on it. Adding a heavy dep to `coop-core` adds that dep's compile time to every build. Put heavy deps in leaf crates. Use feature flags for optional heavy deps (e.g. `tiktoken-rs` is behind the `tokenizer` feature).

3. **Use minimal tokio features in library crates.** Only `coop-gateway` (the binary) uses `tokio = { features = ["full"] }`. Library crates declare only the features they need (e.g. `["sync", "time", "macros", "rt"]`).

4. **Don't add `reqwest` to new crates.** HTTP/TLS deps are expensive. Keep them in `coop-agent`.

5. **Check incremental build time after structural changes.** Run `touch crates/coop-gateway/src/main.rs && time cargo build` — if it's over 1s, investigate.

## Entry Points
- CLI/TUI: crates/coop-gateway/src/main.rs
- Gateway: crates/coop-gateway/src/gateway.rs
- Traits: crates/coop-core/src/traits.rs
- Types: crates/coop-core/src/types.rs
- Prompt: crates/coop-core/src/prompt.rs
- Test fakes: crates/coop-core/src/fakes.rs

## Development Loop
```bash
# 1. Make changes
# 2. just fmt (or: cargo fmt && taplo fmt)
# 3. cargo build
# 4. cargo test -p <crate>
# 5. just lint (or: cargo clippy --all-targets --all-features -- -D warnings)
```

## Rules

Error: Use `anyhow::Result` for error handling
Config: Any change that can create a config error must also update `config_check::validate_config` — add or adjust the relevant check so `coop check` catches it before the server fails to start
Test: Prefer `tests/` folders within each crate
Test: Use fake/placeholder data only — never real PII
Test: Use fakes from coop-core/src/fakes.rs for trait boundaries
Provider: Implement `Provider` trait — see crates/coop-core/src/traits.rs
Channel: Implement `Channel` trait — see crates/coop-core/src/traits.rs

## Code Quality

Comments: Write self-documenting code — prefer clear names over comments
Comments: Never add comments that restate what code does
Comments: Only comment for complex algorithms, non-obvious logic, or "why" not "what"
Comments: Never comment self-evident operations, getters/setters, constructors, or standard Rust idioms
Simplicity: Don't over-abstract early. Trust Rust's type system.
Simplicity: Don't make things optional that don't need to be — the compiler will enforce
Simplicity: Booleans should default to false, not be optional
Errors: Don't add error context that doesn't add useful information (e.g., `.context("Failed to X")` when error already says it failed)
Logging: Use tracing. Don't over-log — errors and key state transitions only.
Tracing: All new features must include tracing spans and events
Tracing: Use `#[instrument]` or manual `info_span!` on public async functions in gateway, provider, and tool crates
Tracing: Log tool inputs/outputs, API request/response metadata, and session state transitions
Tracing: Use `info!` for key events, `debug!` for details, `trace!` for IPC chatter
Tracing: JSONL traces (`COOP_TRACE_FILE`) are the primary debugging interface for AI agents
Tracing: Console output must be a superset of JSONL trace content
Tracing: When modifying tracing spans or events, verify by running the binary with `COOP_TRACE_FILE=traces.jsonl` and confirming the expected fields appear in the output. A successful `cargo build` is not sufficient verification.

## Tracing (Dev)

Coop uses `tracing` with layered subscribers. Console output is always on. JSONL file output is opt-in via environment variable.

### Quick start
```bash
just trace          # TUI + traces.jsonl
just trace-gateway  # daemon + traces.jsonl
just trace-errors   # grep errors from traces
just trace-tools    # grep tool executions
just trace-turns    # grep agent turns
```

### Environment variables
- `COOP_TRACE_FILE` — path to JSONL trace file (e.g. `traces.jsonl`). Enables file output at `debug` level.
- `RUST_LOG` — filter for both console and file (default: `info` console, `debug` file)

### Span hierarchy
```
route_message → agent_turn → turn_iteration → provider_request
                                             → tool_execute
```

### Reading traces (for AI agents)
The `traces.jsonl` file contains one JSON object per line. Each object has:
- `timestamp`, `level`, `message` — standard fields
- `span` — current span name
- `spans` — full span ancestry list
- `target`, `file`, `line` — source location

Filter with standard tools: `grep`, `jq`, `rg`. The `just trace-*` recipes provide common queries.

Each process run starts with a `"coop starting"` event containing version and PID — use `grep "coop starting"` to find run boundaries in the append-only file.

### Debugging with traces

When debugging coop behavior, always start with the traces:

1. **Check existing traces first.** Look for `traces.jsonl` in the working directory. If it exists, search for spans and events related to the bug — the trace may already contain the evidence you need.
2. **Reproduce with tracing if needed.** If no trace exists or the relevant behavior isn't captured, reproduce the bug with `COOP_TRACE_FILE=traces.jsonl` to capture a full trace of the failing behavior.
3. **Identify the bug in the trace.** Read the JSONL to find where the actual behavior diverges from expected — wrong span fields, missing events, error-level entries, unexpected ordering, etc.
4. **Fix and verify via trace.** After applying a fix, re-run with tracing enabled. Confirm the trace now shows the correct behavior (right spans, right field values, no errors). The trace should map directly to the observable effect.
5. **Verify the effect.** The trace tells you what the code *did* — still verify the user-visible outcome is correct (test output, TUI rendering, API response, etc.).

The goal is trace-driven debugging: traces are the primary evidence, not println or guesswork. If a bug can't be diagnosed from the trace, that's a sign the relevant code path needs better instrumentation.

## Anthropic OAuth (Claude Code Tokens)

Coop supports Anthropic OAuth tokens (`sk-ant-oat*`) from Claude Code subscriptions (Pro/Max). These require a specific calling convention that differs from regular API keys. The implementation lives in `crates/coop-agent/src/anthropic_provider.rs`.

**Token detection:** Tokens containing `sk-ant-oat` are treated as OAuth. Regular `sk-ant-api*` keys use the standard API path.

### Required Headers (OAuth only)

```
authorization: Bearer <token>
anthropic-beta: <see below>
user-agent: claude-cli/<VERSION> (external, cli)
x-app: cli
```

Do NOT send `x-api-key` (that's for regular API keys) or `anthropic-dangerous-direct-browser-access` (that's for browser CORS only, not server-side).

### Beta Flags

The `anthropic-beta` header value depends on whether tools are in the request:

- **Without tools:** `oauth-2025-04-20,interleaved-thinking-2025-05-14`
- **With tools:** `claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14`

The `claude-code-20250219` flag is required when tools are present. The `fine-grained-tool-streaming-2025-05-14` flag is **incompatible** with OAuth and must not be sent.

### URL Query Parameter

OAuth requests must append `?beta=true` to the messages endpoint:

```
POST https://api.anthropic.com/v1/messages?beta=true
```

### Tool Name Prefixing

All tool names must be prefixed with `mcp_` before sending to the API, and the prefix must be stripped from tool names in responses:

- Outbound: `bash` → `mcp_bash`, `read_file` → `mcp_read_file`
- Inbound: `mcp_bash` → `bash`, `mcp_read_file` → `read_file`

This applies to tool definitions, `tool_use` blocks in messages, and `tool_use` blocks in responses.

### System Prompt Identity

The first system block must be the Claude Code identity string:

```json
[
  {
    "type": "text",
    "text": "You are Claude Code, Anthropic's official CLI for Claude.",
    "cache_control": { "type": "ephemeral" }
  },
  {
    "type": "text",
    "text": "<your actual system prompt>",
    "cache_control": { "type": "ephemeral" }
  }
]
```

### Model Name

Strip the `anthropic/` prefix before sending. The API expects bare model IDs like `claude-sonnet-4-20250514`, not `anthropic/claude-sonnet-4-20250514`.

### Thinking Blocks

The `interleaved-thinking` beta may return `{"type": "thinking", "thinking": "..."}` content blocks in responses. These must be handled (deserialized and skipped) without error.

### Version String

The `user-agent` header must contain a plausible Claude Code CLI version. Update `CLAUDE_CODE_VERSION` in `anthropic_provider.rs` when upgrading. Check with `claude --version`.

### Reference

This calling convention was derived from the [opencode-anthropic-auth](https://github.com/anomalyco/opencode-anthropic-auth) project and the [OpenClaw](https://github.com/openclaw/openclaw) codebase, which reverse-engineered the Claude Code OAuth flow.

## Never

Never: Commit personal information, real names, real credentials, or PII
Never: Skip `cargo fmt`
Never: Merge without `cargo clippy --all-targets --all-features -- -D warnings` passing
Never: Edit `Cargo.toml` dependency versions manually when `cargo add` works
Never: Ship features without tracing instrumentation
