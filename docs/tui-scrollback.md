# TUI Inline Scrollback

How the terminal TUI renders conversation content into normal terminal scrollback instead of using an alternate screen buffer.

## Problem

The default ratatui approach uses `EnterAlternateScreen`, which clears the terminal and renders into a separate buffer. When the user exits, all conversation content disappears. This makes it impossible to scroll back through previous messages after the session ends, and it breaks terminal multiplexer (tmux) scrollback workflows.

## Solution: `Viewport::Inline`

We use ratatui's `Viewport::Inline(height)` with a small fixed-height viewport (3 lines) that stays pinned to the bottom of the terminal. All message content is printed above the viewport using `Terminal::insert_before()`, which pushes lines into the terminal's native scrollback buffer.

```
┌──────────────────────────────────────────────┐
│ (terminal scrollback — grows upward)         │
│ 09:15 Connected to reid. Type /quit to exit. │
│ 09:15 you: say hello                         │
│ 09:15 reid:                                  │
│ Hello! I'm here to help.                     │
│ 09:16 you: write a haiku                     │
│ 09:16 reid:                                  │
│ Silent compile waits,                        │
│ Borrow checker finds the flaw—               │
│ Safe code, no regrets.                       │
├──────────────────────────────────────────────┤
│ > _                           ← input area   │
│ ⠋ streaming 3s | connected   ← status bar 1  │
│ agent reid | session main    ← status bar 2  │
└──────────────────────────────────────────────┘
```

### What the viewport renders

The viewport is exactly `VIEWPORT_HEIGHT` (3) lines tall:
- **Input area** — multi-line input with `> ` prefix, can expand up to 10 lines
- **Status bar 1** — loading spinner / idle state, connection status
- **Status bar 2** — agent name, session, model, token count

The viewport redraws at 50ms intervals. Because its height is fixed, redraws don't pollute scrollback.

### What goes into scrollback via `insert_before`

Everything else: system messages, user messages, assistant responses, tool calls, tool output. These are printed above the viewport and become part of the terminal's scroll history — visible during the session by scrolling up, and persistent after exit.

## Key Implementation Details

### Streaming text must be line-buffered

`Terminal::insert_before()` always creates new lines — you cannot append to a previously inserted line. If you insert each streaming token individually, every token appears on its own line.

The fix: buffer streaming text and only emit complete lines (on `\n` boundaries). The partial line at the end is flushed when the turn completes.

Relevant fields in `App`:
- `stream_line_buf: String` — accumulates streaming text until a newline arrives
- `streamed_bytes: usize` — tracks how many bytes of the assistant message have been consumed
- `assistant_streamed: bool` — marks that the message was already printed via streaming (so `drain_flushed` can skip it)

### The assistant prefix prints once

Before the first line of streamed content, the main loop prints a prefix line:

```
09:16 reid:
```

The `stream_prefix_printed` flag in the main loop ensures this only happens once per turn.

### `drain_flushed` skips already-streamed messages

When a turn completes, the assistant message exists in `app.messages` but was already printed token-by-token via streaming. `drain_flushed()` detects this via the `assistant_streamed` flag and advances `flushed_count` past the message without returning it — preventing the message from being printed a second time.

### Event ordering in the main loop matters

The main loop runs in this order each tick:

```
1. Receive async turn events (TextDelta, ToolStart, Done, Error, etc.)
2. Print streaming text (complete lines only) via insert_before
3. Flush remaining partial line on turn end via insert_before
4. Drain completed messages to scrollback via insert_before
5. Draw the viewport (input + status bars)
6. Poll for keyboard/mouse events
```

**Step 3 must happen before step 4.** If `drain_flushed` runs first, it resets the `assistant_streamed` flag, causing the flush-remainder check to be skipped — and the response text is never printed.

### `end_turn()` must not clear the stream buffer

`end_turn()` resets `is_loading`, `streamed_bytes`, and `turn_started`, but must NOT clear `stream_line_buf`. The main loop needs the buffer contents to flush the final partial line after the turn ends. The buffer is cleared by `flush_stream_buf()` (via `std::mem::take`) and by `clear()`.

## Gotchas and things that broke

### Viewport height must be fixed

Early attempts used `Viewport::Inline(terminal_height)` — the viewport covered the whole terminal, and messages were rendered inside it. This caused every 50ms redraw to push a complete viewport frame into scrollback, duplicating the entire conversation on every tick.

Fix: viewport is a fixed 3 lines. All content goes through `insert_before`.

### Dynamic viewport resizing pollutes scrollback

If the viewport height changes between frames (e.g., expanding for multi-line input), ratatui adjusts by emitting lines. Any change in viewport size pushes phantom lines into scrollback.

Fix: keep `VIEWPORT_HEIGHT` constant at 3. The input area can grow within the viewport using ratatui's layout constraints, but the viewport itself doesn't resize.

### Short responses with no newlines

If the assistant response is a single line with no `\n` (common for short answers), `take_stream_lines()` never finds a newline and returns `None` every tick. No prefix is printed, no lines are emitted. When the turn ends, `flush_stream_buf()` has the entire response, but the prefix was never printed.

Fix: the turn-end flush code checks `assistant_streamed` (not `stream_prefix_printed`) and prints the prefix itself if needed before flushing the buffer.

## Files

- `crates/coop-tui/src/app.rs` — `App` state, `drain_flushed()`, `take_stream_lines()`, `flush_stream_buf()`
- `crates/coop-tui/src/ui.rs` — `VIEWPORT_HEIGHT`, `draw()` (viewport only), `format_messages()`, `render_scrollback()`
- `crates/coop-gateway/src/main.rs` — main event loop, `flush_to_scrollback()`, `print_above()`, `print_stream_prefix()`
