# BUG-002: Signal loop dies on any provider error

**Status:** Open
**Found:** 2026-02-08
**Trace:** `bug.jsonl` (last execution, starting at pid 84268)
**Related:** BUG-001 (the trigger)

## Symptom

A single Anthropic API error kills the entire Signal channel. The gateway becomes deaf to all Signal messages until manually restarted. Trace shows:

```
WARN  "signal loop stopped" error="Anthropic API error: 400 Bad Request - ..."
ERROR "SignalWebSocket: Websocket closing: request handler failed"
```

## Root Cause

`run_signal_loop` in `crates/coop-gateway/src/signal_loop.rs` uses `?` to propagate errors from `handle_signal_inbound_once`:

```rust
pub(crate) async fn run_signal_loop(...) -> Result<()> {
    loop {
        handle_signal_inbound_once(&mut signal_channel, router.as_ref()).await?;
    }
}
```

Any error from `dispatch_collect_text` (provider errors, tool errors, etc.) propagates up through `handle_signal_inbound_once` → `run_signal_loop`, killing the entire signal loop task. The websocket handler then has no consumer and closes ~15s later.

## Cascade

1. BUG-001 triggers a 400 from Anthropic
2. Error propagates through `dispatch_collect_text` → `handle_signal_inbound_once` → `?` exits `run_signal_loop`
3. Signal loop task ends
4. Websocket has no message consumer → `"SignalWebSocket: Websocket closing: request handler failed"`
5. Gateway is completely deaf to Signal until restart

## Fix

The loop should catch dispatch/provider errors, log them, and continue. Only fatal errors (channel disconnect, auth failure) should kill the loop:

```rust
loop {
    if let Err(error) = handle_signal_inbound_once(&mut signal_channel, &router).await {
        tracing::error!(%error, "signal message handling failed");
        // continue — don't kill the whole channel
    }
}
```

Distinguish fatal vs transient errors if needed (e.g. channel recv errors are fatal, provider errors are transient).
