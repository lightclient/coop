# BUG-003: Signal reply sent out of order after tool call

**Status:** Fixed
**Found:** 2026-02-07

## Symptom

When the agent produces text before invoking `signal_reply` (e.g. "Now I have the full picture. Let me reply:"), that text arrives on Signal *after* the tool's actual reply — reversing the intended order.

## Timeline (from trace)

1. `06:19:51.590` — Iteration 6 completes: `response_text_len: 42` ("Now I have the full picture. Let me reply:") + `tool_use: signal_reply`
2. `06:19:51.591` — `signal_reply` tool executes, queues `Reply` action with the long analysis
3. `06:19:51.717` — Reply (long analysis) sent over Signal ✅
4. `06:19:53.490` — Iteration 7 returns `end_turn` (empty text). Turn completes.
5. `06:19:53.559` — `dispatch_collect_text` returns "Now I have the full picture. Let me reply:" → `signal_loop` sends it as `send_text` **after** the reply ❌

## Root Cause

`signal_loop.rs` used `router.dispatch_collect_text()`, which accumulates **all** `TextDelta` events across every iteration of the agent turn into one string, then sends it as a single message after the turn completes.

Meanwhile, tools like `signal_reply` send messages directly via the `action_tx` channel during the turn. The result: tool replies arrive first, then the preamble text arrives late.

Both `Channel::send` (wraps as `SignalAction::SendText`) and the tool's `action_tx.send(Reply)` flow through the **same** MPSC channel, which preserves insertion order. The bug is purely about *when* the text gets enqueued — after the entire turn instead of before the tool.

## Fix

Replaced `dispatch_collect_text` in `signal_loop.rs` with inline event processing. The signal loop now processes `TurnEvent`s directly:

- **`TextDelta`** → accumulated into a buffer
- **`ToolStart`** → **flushes** the buffer to the channel *before* the tool executes
- **`Error`** → replaces the buffer with the error message
- **`Done`** → breaks the loop; remaining text flushed after

A `flush_text` helper sends non-empty text to the channel and clears the buffer.

### Why ordering is preserved

The gateway emits events in this order: `TextDelta`(s) → `ToolStart` → tool executes → `ToolResult`. The signal loop receives `ToolStart` and flushes accumulated text via `Channel::send` (→ `action_tx.send(SendText)`) before the tool gets to `action_tx.send(Reply)`. Since both paths converge on the same MPSC channel, whoever enqueues first wins — and the flush has a head start because the tool must still deserialize arguments, parse the target, and trace before sending.

### Corrected timeline

1. `TextDelta("Now I have the full picture...")` → accumulated
2. `ToolStart(signal_reply)` → flush text to channel immediately
3. Tool executes → sends `Reply` action (arrives after flush)
4. `Done` → flush remaining text (empty)

### Scope

- `dispatch_collect_text` remains in `router.rs` for scheduler/cron use, where the ordering issue does not apply (cron delivers to a different target than the tool's side-effects).
- No changes to `coop-core` types.

## Test coverage

- `text_before_tool_call_is_flushed_separately` — scripted provider returns text + `signal_reply` tool call in iteration 1, then text in iteration 2. Verifies pre-tool and post-tool text arrive as 2 separate outbound messages (not concatenated into one message sent at the end).
- All existing `signal_loop` tests continue to pass (filtering, empty response suppression, tool event emission, typing indicators).
