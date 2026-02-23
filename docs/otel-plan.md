# OpenTelemetry Tracing for Coop — Implementation Plan

Heavy instrumentation for AI agent debugging loops. Agents read `traces.jsonl` directly. Console logging is a human-friendly superset of what the JSONL traces contain. Zero overhead when tracing env vars are unset.

## Design Principles

1. **JSONL is the primary output** — AI agents consume `traces.jsonl` directly. Every trace, span, and event must be machine-parseable NDJSON.
2. **Console is a superset** — anything in JSONL also appears on console (at appropriate level). Console may additionally show human-friendly formatting, colors, etc.
3. **No substantial features without traces** — all new work must include OpenTelemetry instrumentation. This is enforced by code review and documented in AGENTS.md.
4. **Activated by environment** — `COOP_TRACE_FILE=traces.jsonl` enables file output. `OTEL_EXPORTER_OTLP_ENDPOINT` enables OTLP export. Without either, behavior is identical to today.
5. **PII is acceptable in traces** — these are local dev files, not shipped to production telemetry backends.

## Testing Strategy

### Phase 1 test: JSONL plumbing
One integration test in `crates/coop-gateway/tests/tracing_test.rs` that:
- Sets `COOP_TRACE_FILE` to a tempfile
- Calls `tracing_setup::init()` (or a test-friendly variant)
- Emits a span and event via `tracing` macros
- Drops the guard / flushes
- Reads the tempfile and asserts each line is valid JSON with expected fields (`timestamp`, `level`, `span`, `spans`)

### Phase 2 test: instrumentation coverage
Integration test in `crates/coop-gateway/tests/tracing_integration.rs` that:
- Uses `FakeProvider` and `FakeExecutor` from `coop-core/src/fakes.rs`
- Runs a turn through `Gateway::run_turn_with_trust()`
- Captures trace output to a tempfile
- Asserts key spans appear: `agent_turn`, `turn_iteration`, `provider_request`, `tool_execute`
- Asserts key fields are present (session, tool name, token counts)

### What we skip testing
- Console output formatting (visual, not worth automating)
- OTLP export (requires running collector, tested manually with `just trace-jaeger`)

## Implementation Steps

### Phase 1: Foundation

#### 1.1 Workspace dependencies (`Cargo.toml`)

Add to `[workspace.dependencies]`:
```toml
tracing-appender = "0.2"
opentelemetry = { version = "0.28", features = ["trace"] }
opentelemetry_sdk = { version = "0.28", features = ["rt-tokio"] }
opentelemetry-otlp = { version = "0.28" }
tracing-opentelemetry = { version = "0.28" }
```

#### 1.2 Crate dependency changes

**`crates/coop-core/Cargo.toml`** — add `tracing = { workspace = true }`. Core stays OTEL-free; only emits `tracing` spans/events.

**`crates/coop-gateway/Cargo.toml`** — add:
```toml
tracing-appender = { workspace = true }

[features]
default = []
otel = [
  "dep:opentelemetry",
  "dep:opentelemetry_sdk",
  "dep:opentelemetry-otlp",
  "dep:tracing-opentelemetry",
]
```

#### 1.3 Tracing subscriber setup (`crates/coop-gateway/src/tracing_setup.rs`)

New module. Builds a layered `tracing_subscriber::Registry`:

| Layer | Activation | Filter | Format |
|-------|-----------|--------|--------|
| **Console** | Always | `RUST_LOG` (default `info`) | `fmt()` with target=false (current behavior) |
| **JSONL file** | `COOP_TRACE_FILE` env var | `RUST_LOG` or default `debug` | `fmt::layer().json()` with `FmtSpan::FULL`, `with_span_list(true)`, `with_file(true)`, `with_line_number(true)`. Writer via `tracing_appender::rolling::never()` |
| **OTLP export** | `#[cfg(feature = "otel")]` + `OTEL_EXPORTER_OTLP_ENDPOINT` | `debug` | tonic gRPC exporter, batch span processor |

Key: JSONL layer uses the same `RUST_LOG` filter as console, falling back to `debug` if unset. Console is always >= JSONL verbosity.

Returns a guard that must be held alive in `main()` for non-blocking writer flush on shutdown.

```rust
pub fn init() -> Result<TracingGuard> { ... }
pub fn shutdown() { ... } // flush OTLP batches on exit
```

#### 1.4 Wire into main (`crates/coop-gateway/src/main.rs`)

- Add `mod tracing_setup;`
- Replace `tracing_subscriber::fmt()...init()` with `let _tracing_guard = tracing_setup::init()?;`
- Call `tracing_setup::shutdown()` on clean exit paths

### Phase 2: Gateway & Provider Instrumentation

#### 2.1 Gateway spans (`crates/coop-gateway/src/gateway.rs`)

```
agent_turn                          (session, input_len, trust)
├── turn_iteration                  (iteration, max_iterations)
│   ├── provider_request            (message_count, tool_count, streaming)
│   │   └── [streaming events at debug level]
│   ├── tool_execute                (tool.name, tool.id)    ← per tool call
│   │   └── info: output_len, is_error, output_preview
│   └── info: has_tool_requests, response_text_len
└── info: total_input_tokens, total_output_tokens, hit_limit
```

Concrete changes:
- `run_turn_with_trust()` — wrap body in `info_span!("agent_turn", session = %session_key, input_len = user_input.len(), trust = ?trust)`
- Inner loop — `info_span!("turn_iteration", iteration, max = turn_config.max_iterations)`
- `assistant_response_streaming()` / `_non_streaming()` — `info_span!("provider_request", message_count = messages.len(), tool_count = tool_defs.len(), streaming)`
- Each tool execution — `info_span!("tool_execute", tool.name = %req.name, tool.id = %req.id)`, then `info!(output_len, is_error, output_preview = &output.content[..500.min(output.content.len())])`
- After turn — `info!(input_tokens = total_usage.input_tokens, output_tokens = total_usage.output_tokens, hit_limit)`

#### 2.2 Provider spans (`crates/coop-agent/src/anthropic_provider.rs`)

- `complete()` / `stream()` — `info_span!("anthropic_request", model = %self.model, method)` with `message_count`, `tool_count`
- HTTP attempt loop — `debug_span!("http_attempt", attempt)` with status code, retry backoff
- SSE events — `debug!` on `message_start` (input_tokens), `content_block_start` (type), `message_stop`
- Response parsing — `info!(input_tokens, output_tokens, stop_reason)`

#### 2.3 Router spans (`crates/coop-gateway/src/router.rs`)

- `dispatch()` — `info_span!("route_message", session = %key, trust = ?trust, source)`
- Trust resolution — `debug!(resolved_trust = ?trust, user, situation)`

### Phase 3: Tool & Core Instrumentation

#### 3.1 Tool executor (`crates/coop-core/src/tools/mod.rs`)

- `DefaultExecutor::execute()` — `info_span!("tool_execute", tool = %name)`, log full `arguments` at `debug!`

#### 3.2 Individual tools

**`bash.rs`** — `debug_span!("bash")` with `command`, `workspace`. After: `info!(exit_code, stdout_len, stderr_len)`

**`read_file.rs`** — `debug_span!("read_file")` with `path`. After: `debug!(bytes_read)`

**`write_file.rs`** — `debug_span!("write_file")` with `path`, `content_len`. After: `debug!(written)`

**`list_directory.rs`** — `debug_span!("list_directory")` with `path`. After: `debug!(entry_count)`

#### 3.3 IPC (`crates/coop-ipc/`)

- `IpcServer::accept()` — `debug_span!("ipc_accept")`
- `IpcConnection::send/recv` — `trace!` level (very chatty)

### Phase 4: DX, Docs & Justfile

#### 4.1 Justfile recipes

```just
# Run TUI with JSONL tracing to traces.jsonl
trace:
    COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- chat

# Run gateway daemon with JSONL tracing
trace-gateway:
    COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start

# Run with OTLP export to Jaeger (localhost:4317)
trace-jaeger:
    OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 \
    COOP_TRACE_FILE=traces.jsonl \
    cargo run --bin coop --features otel -- chat

# Tail recent trace events
trace-tail n="50":
    tail -n {{n}} traces.jsonl

# Show errors from traces
trace-errors:
    grep '"level":"ERROR"' traces.jsonl | tail -20

# Show warnings from traces
trace-warnings:
    grep '"level":"WARN"' traces.jsonl | tail -20

# Show tool execution spans
trace-tools:
    grep '"tool_execute"' traces.jsonl | tail -20

# Show API request spans
trace-api:
    grep '"anthropic_request"' traces.jsonl | tail -20

# Show agent turn spans
trace-turns:
    grep '"agent_turn"' traces.jsonl | tail -20

# Clear trace file
trace-clear:
    rm -f traces.jsonl
```

#### 4.2 .gitignore

Add:
```
traces.jsonl
*.jsonl
```

#### 4.3 AGENTS.md additions

Add to Rules section:
```
Tracing: All new features must include OpenTelemetry spans and events
Tracing: Use `#[instrument]` on public async functions in gateway, provider, and tool crates
Tracing: Log tool inputs/outputs, API request/response metadata, and session state transitions
Tracing: Use `info!` for key events, `debug!` for details, `trace!` for IPC chatter
Tracing: JSONL traces (`COOP_TRACE_FILE`) are the primary debugging interface for AI agents
Tracing: Console output must be a superset of JSONL trace content
Never: Ship features without tracing instrumentation
```

Add new section:
```markdown
## Tracing (Dev)

Coop uses `tracing` with layered subscribers. Console output is always on. JSONL and OTLP are opt-in.

### Quick start
\`\`\`bash
just trace          # TUI + traces.jsonl
just trace-gateway  # daemon + traces.jsonl
just trace-errors   # grep errors from traces
just trace-tools    # grep tool executions
\`\`\`

### Environment variables
- `COOP_TRACE_FILE` — path to JSONL trace file (e.g. `traces.jsonl`). Enables file output.
- `RUST_LOG` — filter for both console and file (default: `info` console, `debug` file)
- `OTEL_EXPORTER_OTLP_ENDPOINT` — OTLP gRPC endpoint (requires `--features otel`)

### Span hierarchy
\`\`\`
agent_turn → turn_iteration → provider_request → http_attempt
                             → tool_execute
route_message → agent_turn
\`\`\`

### Reading traces (for AI agents)
The `traces.jsonl` file contains one JSON object per line. Each object has:
- `timestamp`, `level`, `message` — standard fields
- `span` — current span name
- `spans` — full span ancestry list
- `target`, `file`, `line` — source location

Filter with standard tools: `grep`, `jq`, `rg`. The `just trace-*` recipes provide common queries.
```

## Verification

1. `just check` passes (fmt, clippy, deny, test)
2. `just trace` → send a message → `traces.jsonl` contains spans: `agent_turn`, `turn_iteration`, `tool_execute`, `anthropic_request`
3. `just run` (without env vars) — identical to before, no file, no overhead
4. `cargo build --features otel` compiles; without `OTEL_EXPORTER_OTLP_ENDPOINT`, no connection attempted
5. Every `just trace-*` recipe returns relevant filtered output
