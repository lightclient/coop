# Fix Signal Integration Issues

Trace analysis from the 2026-02-07 22:38–22:40 session surfaced five issues.
Three require code changes, two are configuration/filter changes.

## Issue 1 — Stale 422 from Signal websocket at startup

**Severity:** WARN (1 occurrence, at connect time)
**Source:** `libsignal_service::websocket` line 172

### Root cause

When coop starts and opens the Signal websocket, the server delivers a
queued response (status 422) for a request ID from the previous session.
The websocket handler can't find a matching pending request, so it logs
the warning. This means the previous session didn't cleanly close its
websocket (likely killed via signal/ctrl-c without graceful shutdown).

### Fix

**File:** `crates/coop-channels/src/signal.rs` — `start_signal_runtime()`
and `crates/coop-gateway/src/main.rs`

Add a graceful shutdown path:

1. When `main.rs` receives a shutdown signal (SIGINT/SIGTERM), send a
   shutdown message through the action channel before dropping it.

2. In `send_task()`, on receiving the shutdown sentinel, call
   `manager.disconnect()` (or let the websocket close cleanly) before
   returning.

3. In `receive_task()`, detect the cancellation and break out of the
   receive loop cleanly.

The websocket 422 itself is harmless and comes from the Signal server, so
it can't be fully prevented if the previous process was killed. But clean
shutdown on the *current* process will prevent the *next* startup from
seeing it.

Additionally, the WARN can be demoted in the tracing filter since it's
a known benign condition from a third-party library:

```rust
EnvFilter::new("debug,libsignal_service::websocket=error")
```

### Verification

- Start coop, send a message, shut down with ctrl-c
- Restart coop, confirm no 422 WARN in the first 5 seconds of traces
- Kill -9 coop, restart, confirm the WARN appears (expected—ungraceful)

---

## Issue 2 — `stop_reason` missing from provider response tracing

**Severity:** instrumentation gap (all 5 provider responses affected)

### Root cause

In `gateway.rs`, `assistant_response_streaming()` and
`assistant_response_non_streaming()` log `provider response complete` with
`input_tokens` and `output_tokens`, but never include `stop_reason`:

```rust
info!(
    input_tokens = usage.input_tokens,
    output_tokens = usage.output_tokens,
    "provider response complete"
);
```

The `stop_reason` (end_turn / tool_use / max_tokens) is not captured from
the provider response and not propagated through the `Usage` or `Message`
types.

### Fix

**Step 1 — Propagate stop_reason through the type system.**

**File:** `crates/coop-core/src/types.rs`

Add `stop_reason: Option<String>` to the `Usage` struct (or return it as a
separate field alongside `Usage`). The provider is the source of truth.

**File:** `crates/coop-agent/src/anthropic_provider.rs`

When parsing the streaming response's `message_delta` event (which contains
`stop_reason`) or the non-streaming response body, extract `stop_reason`
and include it in the returned `Usage`.

**Step 2 — Log it.**

**File:** `crates/coop-gateway/src/gateway.rs`

In both `assistant_response_streaming()` and
`assistant_response_non_streaming()`, add `stop_reason` to the
`provider response complete` event:

```rust
info!(
    input_tokens = usage.input_tokens,
    output_tokens = usage.output_tokens,
    stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
    "provider response complete"
);
```

### Verification

Run with `COOP_TRACE_FILE=traces.jsonl`, send a text message (should
produce `stop_reason: "end_turn"`) and a message that triggers tool use
(should produce `stop_reason: "tool_use"`). Confirm both values appear in
the JSONL:

```bash
grep "provider response complete" traces.jsonl | jq '.fields.stop_reason'
```

---

## Issue 3 — Excessive receiver chain trimming

**Severity:** cosmetic / minor perf (14 trims in 2 minutes)

### Root cause

Every send/receive cycle creates a new Double Ratchet step. With 4 device
sub-sessions (`.1` through `.4`) and frequent message exchange (typing
start, typing stop, message, delivery receipt, read receipt), the chain
count hits the trim threshold (6) on nearly every inbound message. This
is correct Signal Protocol behavior—the library is pruning old receiver
chains to bound memory.

The trim threshold of 6 is hardcoded in `libsignal-service-rs`. The
trimming itself is cheap (removes old chain state from memory), but the
INFO-level logging is noisy.

### Fix

No code change required in coop. This is working as designed.

**Reduce trace noise** by filtering the libsignal crypto targets to a
higher level. See Issue 5 below—the same filter change addresses this.

---

## Issue 4 — Trace noise from Signal crypto spans

**Severity:** usability (550+ of 1603 lines are low-level crypto ops)

### Root cause

The `libsignal_service` and `libsignal_protocol` crates emit INFO/DEBUG
span events for every encrypt, decrypt, sealed_sender_decrypt, and
open_envelope operation. With `FmtSpan::NEW | FmtSpan::CLOSE` enabled on
the JSONL layer, each crypto operation produces 2+ lines with null messages.

Counts from the session:
- 936 `encrypt` span events
- 488 `sealed_sender_decrypt` span events
- 347 `decrypt` span events
- 335 `open_envelope` span events

This drowns out the application-level trace.

### Fix

**File:** `crates/coop-gateway/src/tracing_setup.rs`

Add target-level filters to suppress the noisy upstream crates. Replace the
blanket `debug` default with targeted directives:

```rust
let jsonl_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
    EnvFilter::new(
        "debug,\
         libsignal_service=warn,\
         libsignal_protocol=warn,\
         presage=info"
    )
});
```

This keeps coop's own crates at `debug`, bumps libsignal to `warn` (only
actual errors), and keeps presage at `info` (connection lifecycle events
are still useful).

For the console layer, apply the same filter if `RUST_LOG` is not set:

```rust
let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
    EnvFilter::new(
        "info,\
         libsignal_service=warn,\
         libsignal_protocol=warn"
    )
});
```

Users can still override with `RUST_LOG=debug,libsignal_service=debug` if
they need to debug Signal protocol issues.

### Verification

Run with `COOP_TRACE_FILE=traces.jsonl`, exchange a few messages, then:

```bash
# Should be zero or near-zero
grep -c '"encrypt"\|"decrypt"\|"sealed_sender"' traces.jsonl

# Application flow should still be fully visible
grep "route_message\|agent_turn\|provider_request\|tool_execute" traces.jsonl | wc -l
```

---

## Implementation order

| Priority | Issue | Effort | Crate |
|----------|-------|--------|-------|
| 1 | #5 trace noise filter | 10 min | coop-gateway |
| 2 | #3 stop_reason tracing | 30 min | coop-core, coop-agent, coop-gateway |
| 3 | #1 typing sync error | 1–2 hr | coop-channels (+ presage fork) |
| 4 | #2 graceful shutdown | 1 hr | coop-channels, coop-gateway |

Issue 4 (chain trimming) requires no code change—it's resolved by issue 5's
filter.

Issues 5 and 3 are independent and can be done in parallel. Issue 1 depends
on evaluating the presage fork approach. Issue 2 can be deferred since it's
a cosmetic startup warning.
