# TUI Scrollback via Fixed Viewport

How the terminal TUI renders conversation content using a small fixed viewport with native terminal scrollback.

## Problem

A full-screen `Viewport::Inline(terminal_height)` cannot support terminal scrollback during the session because `insert_before` pushes the top of the viewport's own rendered content into scrollback, leaking status bars and duplicating messages.

## Solution: Fixed-Height Viewport with Native Scrollback

The viewport is a thin control area (input + status bars) pinned to the bottom of the terminal. All message content is printed above it via `insert_before` and becomes native terminal scrollback — exactly like Claude Code and other streaming TUI apps.

```
┌──────────────────────────────────────────────┐
│ (terminal scrollback — native, scrollable)   │
│ 09:15 Connected to reid. Type /quit to exit. │
│                                              │
│ 09:15 you: say hello                         │
│                                              │
│ 09:15 reid: Hello! I'm here to help.         │
│                                              │
│ 09:16 you: write a haiku                     │
│                                              │
│ 09:16 reid:                                  │  ← all messages in scrollback
│ Silent compile waits,                        │
│ Borrow checker finds the flaw—               │
│ Safe code, no regrets.                       │
├──────────────────────────────────────────────┤
│ > _                           ← input        │  ← viewport (fixed height)
│ ⠋ streaming 3s | connected   ← status bar 1  │
│ agent reid | session main    ← status bar 2  │
└──────────────────────────────────────────────┘
```

### What the viewport renders

`Viewport::Inline(VIEWPORT_HEIGHT)` — a fixed-height viewport containing only:
- **Input area** — multi-line input with `> ` prefix, can expand up to 10 lines
- **Status bar 1** — loading spinner / idle state, connection status
- **Status bar 2** — agent name, session, model, token count

Messages are never rendered inside the viewport. They live in native terminal scrollback.

### How messages reach scrollback

Completed messages are drained via `drain_flushed()` and printed above the viewport via `insert_before`. The user scrolls up in their terminal (or tmux) to see history.

### Streaming

Streaming text is line-buffered because `insert_before` always creates new lines — you cannot append to a previously inserted line. Tokens are buffered in `stream_line_buf` and only emitted as complete lines (on `\n` boundaries). The partial line is flushed when the turn completes.

Required fields on `App`:
- `streamed_bytes: usize` — how many bytes of the streaming assistant message have been consumed
- `stream_line_buf: String` — accumulates text until a newline arrives
- `assistant_streamed: bool` — marks that the message was already printed via streaming (so `drain_flushed` skips it to avoid double-printing)

The assistant name prefix (e.g. `09:16 reid: `) is printed once before the first streamed line via `print_stream_prefix()`.

### Exit

On exit, the full conversation is already in scrollback. `flush_to_scrollback()` drains any remaining messages, then the terminal is restored.

## Main Loop Event Ordering

The order is critical — step 3 must happen before step 4:

```
1. Receive async turn events (TextDelta, ToolStart, Done, Error, etc.)
2. Print streaming text (complete lines only) via insert_before
3. Flush remaining partial line on turn end via insert_before
4. Drain completed messages to scrollback via insert_before
5. Draw the viewport (input + status bars only)
6. Poll for keyboard/mouse events
```

## Files

- `crates/coop-tui/src/app.rs` — `App` state, streaming methods (`take_stream_lines`, `flush_stream_buf`), `drain_flushed()`
- `crates/coop-tui/src/ui.rs` — `draw()` (viewport only), `format_messages()`, `render_scrollback()`, `VIEWPORT_HEIGHT`
- `crates/coop-gateway/src/main.rs` — main event loop, `flush_to_scrollback()`, `print_stream_prefix()`
