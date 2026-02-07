# Signal Rich Tracing (JSONL-only)

Add rich tracing for Signal message handling using the existing `tracing` + JSONL pipeline. **Do not add OpenTelemetry, OTLP exporters, or any new telemetry backend.**

Read `AGENTS.md` at the project root before starting. Follow the development loop and tracing rules.

## Goal

Improve observability for Signal rich message flows (inbound parsing, filtering, tool actions, send path, typing lifecycle) by emitting structured `tracing` spans/events that appear in `COOP_TRACE_FILE` JSONL output.

This work is **trace-only**:
- ✅ Extend existing spans/events and fields
- ✅ Keep console + JSONL behavior aligned
- ❌ No OpenTelemetry integration
- ❌ No `tracing-opentelemetry`/OTLP dependencies

## Raw content policy

For this prompt, traces should include **raw Signal message text/content by default** (not redacted).

Specifically include raw content in trace fields for:
- inbound parsed content
- outbound text payloads
- reply text payloads
- reaction emoji

Also include metadata fields alongside raw content (kind, sender, target, timestamps, group/dm).

## Scope

Relevant files:
- `crates/coop-channels/src/signal.rs`
- `crates/coop-channels/src/signal/inbound.rs`
- `crates/coop-channels/src/signal_tools.rs`
- `crates/coop-gateway/src/main.rs` (signal loop filter path)
- `crates/coop-gateway/src/gateway.rs` (typing notifier lifecycle)
- `crates/coop-gateway/src/tracing_setup.rs` (only if needed for existing JSONL fields; no new backend)

## Required tracing improvements

### 1) Inbound receive and parse tracing

Add spans/events around inbound Signal processing.

In receive path (`signal.rs`):
- Add span: `signal_receive_event`
- Fields:
  - `signal.sender`
  - `signal.content_body` (variant name)
  - `signal.timestamp` (message timestamp)

In parser (`signal/inbound.rs`):
- Add span: `signal_inbound_parse`
- Fields (when available):
  - `signal.inbound_kind`
  - `signal.chat_id`
  - `signal.is_group`
  - `signal.message_timestamp`
  - `signal.raw_content` (final formatted inbound content)
- Emit explicit events for outcomes:
  - parsed + emitted
  - unsupported body variant
  - dropped/empty

### 2) Gateway filtering tracing

In `run_signal_loop` filtering path (`main.rs`):
- When filtering typing/receipt, emit event:
  - message: `signal inbound filtered`
  - fields:
    - `signal.inbound_kind`
    - `signal.sender`
    - `signal.chat_id`
    - `signal.message_timestamp`
    - `signal.raw_content`

- When dispatching, emit event:
  - message: `signal inbound dispatched`
  - same fields

### 3) Signal action send tracing

In `send_signal_action` and send helpers (`signal.rs`):
- Add span: `signal_action_send`
- Required fields:
  - `signal.action` (`send_text|react|reply|typing`)
  - `signal.target_kind` (`direct|group`)
  - `signal.target`
  - `signal.timestamp`

Per action include raw payload fields:
- SendText: `signal.raw_content`
- Reply: `signal.raw_content`, `signal.quote_timestamp`, `signal.quote_author_aci`
- React: `signal.emoji`, `signal.remove`, `signal.target_sent_timestamp`, `signal.target_author_aci`
- Typing: `signal.started`

Emit success/failure events:
- `signal action sent`
- `signal action send failed` (with error)

### 4) Tool execution tracing detail

In `signal_tools.rs`, add structured events inside tool execute:
- `signal tool action queued`
- fields:
  - `tool.name`
  - `signal.action`
  - `signal.chat_id`
  - raw payload (`text`/`emoji`)
  - timestamps + author ids

### 5) Typing lifecycle tracing in gateway

In `gateway.rs`:
- When typing starts: event `typing notifier start`
- When typing stops (guard/drop): event `typing notifier stop`
- Fields:
  - `session`
  - `signal.started`
  - resolved target info if available

## Field naming conventions

Use stable snake_case-like field keys under a `signal.` namespace where possible, e.g.:
- `signal.raw_content`
- `signal.inbound_kind`
- `signal.content_body`
- `signal.message_timestamp`
- `signal.chat_id`
- `signal.target`
- `signal.action`

Do not introduce inconsistent aliases for the same value.

## Non-goals

- No OpenTelemetry code or deps
- No metrics backend changes
- No redaction-by-default behavior for this task
- No protocol behavior changes beyond tracing instrumentation

## Tests

Add/extend tests to confirm trace-relevant behavior paths execute:

### coop-channels
- Parser tests should continue covering variants and can assert no behavior regressions.
- Tool tests should still validate action construction.

### coop-gateway
- Keep filter tests for typing/receipt.
- Keep typing guard tests.

(Trace content itself will be validated manually via JSONL verification below.)

## Verification (required)

After implementation run:

```bash
cargo fmt
cargo build --features signal
cargo test -p coop-channels --features signal
cargo test -p coop-gateway --features signal
cargo clippy --all-targets --all-features -- -D warnings
```

Then verify JSONL traces directly:

```bash
COOP_TRACE_FILE=traces.jsonl cargo run -p coop-gateway --features signal -- version
```

And inspect fields/events in traces:

```bash
rg "signal_" traces.jsonl
rg "signal inbound filtered|signal inbound dispatched|signal action" traces.jsonl
```

Confirm that new trace entries include:
- raw content fields by default (`signal.raw_content`)
- inbound kind/body metadata
- action metadata + payload details
- typing start/stop lifecycle events

## Implementation order

1. Add inbound receive/parse spans/events (`signal.rs`, `signal/inbound.rs`)
2. Add gateway filter dispatch events (`main.rs`)
3. Add action send spans/events (`signal.rs`)
4. Add tool action queued events (`signal_tools.rs`)
5. Add typing lifecycle events (`gateway.rs`)
6. Run fmt/build/test/clippy
7. Verify JSONL fields/events with `COOP_TRACE_FILE`
