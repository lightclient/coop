# Prompt: Reverse-Engineer the Pi TUI and Rebuild It in Rust for Coop

## Objective

You will reverse-engineer the **pi coding agent TUI** (`@mariozechner/pi-coding-agent` interactive mode) by running it in tmux and methodically interrogating every visual element, interaction, and behavioral detail. You will then build a **pixel-perfect replica** in Rust that connects to the **coop gateway**.

The key architectural insight: **drop ratatui**. Pi's TUI engine is line-based — components return `Vec<String>` of ANSI-styled lines, containers concatenate children vertically, and a differential renderer writes them to the terminal. Build an equivalent engine in Rust on top of `crossterm`. This makes the port a near-1:1 structural translation rather than fighting ratatui's 2D cell buffer paradigm.

**Follow pi's architecture as closely as possible.** Do not redesign, do not improve, do not abstract differently. The goal is a faithful structural port. When pi uses a `Container` with `addChild`, you use a `Container` with `add_child`. When pi tracks `maxLinesRendered` and `previousViewportTop`, you track `max_lines_rendered` and `previous_viewport_top`. When in doubt about how to structure something, read the pi source and match it. Deviating from pi's architecture means deviating from pi's behavior, and the whole point is identical behavior.

Assume all details are meaningful. Don't just assume text is static text, try to interpret and determine if it actually raising important information to the user that needs to be plugged into coop later. For example "claude-opus-4-6" isn't static text, it's the user's model!

Be sure to review the commands available in the tui. Replicate the slash / command structure, especially like login and settings. Have control key sequences to toggle tool calls / thinking, etc.

---

## Architecture: Pi's TUI Engine

Pi's rendering model (from `/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/tui.js`, 928 lines) is:

```
Component trait:
  render(width) -> string[]      // returns ANSI-styled lines, each ≤ width
  handleInput?(data) -> void     // keyboard input when focused
  invalidate() -> void           // clear cached render state

Container extends Component:
  children: Component[]
  render(width) -> concat all children's render(width) output

TUI extends Container:
  - Renders all children to get lines
  - Diffs against previous frame
  - Writes only changed lines using cursor movement + line clear
  - Synchronized output (CSI ?2026h/l) to prevent flicker
  - Tracks cursor position, viewport scrolling
  - Overlay compositing for modal dialogs
```

**This is NOT alternate-screen.** Pi uses inline rendering — content grows upward into terminal scrollback as the conversation gets longer. The TUI owns a working area at the bottom of the terminal whose size is tracked by `maxLinesRendered`. When content grows beyond the terminal height, older lines scroll into native terminal scrollback — the user can scroll up with their terminal's scroll (mouse wheel, Shift+PageUp, etc.) to see them. This is critical: messages are never lost, they just scroll up. The layout from top to bottom is:

```
[terminal scrollback — old messages scroll up here naturally]
[headerContainer — welcome text, context info]
[chatContainer — messages (user, assistant, tool calls)]
[pendingMessagesContainer]
[statusContainer — spinner/loader]
[widgetContainerAbove — extension widgets]
[editorContainer — the input editor with ─── borders]
[widgetContainerBelow — extension widgets]  
[footer — pwd, token stats, model name]
```

The editor component (`editor.js`, 1712 lines) renders:
- Top border: `────────────` (full width, colored with thinking-level border color)
- Content lines with cursor (inverse video for cursor character)
- Bottom border: `────────────`
- Scroll indicators: `─── ↑ N more ───` / `─── ↓ N more ───`

The footer (`footer.js`) renders 2 lines:
- Line 1: `~/path/to/project (branch)` — dim
- Line 2: `↑3 ↓189 R8.6k W4.2k $0.031 (sub) 2.2%/200k (auto)` left-aligned, `claude-opus-4-6 • medium` right-aligned — dim

User messages render as markdown with background color `#343541`. Assistant messages render as markdown with no background. Thinking blocks render as italic gray markdown. Tool calls render inside a `Box` component with background colors (`#283228` success, `#3c2828` error, `#282832` pending).

## Exact Colors (from dark.json)

The dark theme lives at:
`/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/theme/dark.json`

Key resolved RGB values:
```
accent:           #8abeb7
border:           #5f87ff  (editor border default)
borderAccent:     #00d7ff
borderMuted:      #505050
success:          #b5bd68
error:            #cc6666
warning:          #ffff00
muted/gray:       #808080
dim:              #666666
darkGray:         #505050

userMessageBg:    #343541
toolPendingBg:    #282832
toolSuccessBg:    #283228
toolErrorBg:      #3c2828

mdHeading:        #f0c674
mdLink:           #81a2be
mdCode/accent:    #8abeb7
mdListBullet:     #8abeb7

thinkingText:     #808080
thinkingMedium:   #81a2be  (default border color seen in captures)

Syntax: comment=#6A9955 keyword=#569CD6 function=#DCDCAA
        variable=#9CDCFE string=#CE9178 number=#B5CEA8
        type=#4EC9B0 operator=#D4D4D4 punctuation=#D4D4D4
```

All colors are 24-bit RGB, emitted as `\x1b[38;2;r;g;bm` (fg) and `\x1b[48;2;r;g;bm` (bg).

---

## Phase 0: Setup and Orientation

### Read the coop codebase first
1. Read `AGENTS.md` at the project root — follow all rules, especially the privacy/PII rules.
2. Read every file in `crates/coop-tui/src/` — this is the **existing** coop TUI you will be replacing.
3. Read `crates/coop-gateway/src/main.rs` — this is how the TUI connects to the gateway. Your replacement must preserve the integration pattern (mpsc channels, `TurnEvent` enum, `Gateway::run_turn`).
4. Read `crates/coop-core/src/types.rs` and `crates/coop-core/src/traits.rs` for the data model.

### Read the pi source code
These are the key files to understand the rendering architecture:

```
# Core TUI engine — differential renderer, Container, overlay compositing
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/tui.js (928 lines)

# Editor component — multi-line input, word wrap, cursor, borders, autocomplete
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/components/editor.js (1712 lines)

# Markdown renderer
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/components/markdown.js

# Basic components
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/components/text.js
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/components/box.js
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/components/spacer.js

# String utilities — visibleWidth, truncateToWidth, wrapTextWithAnsi
/root/.bun/install/global/node_modules/@mariozechner/pi-tui/dist/utils.js

# Interactive mode — top-level layout, event handling, component wiring
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/interactive-mode.js (3673 lines)

# Message components
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/components/user-message.js
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/components/assistant-message.js
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/components/tool-execution.js

# Footer
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/components/footer.js

# Dark theme
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/theme/dark.json
/root/.bun/install/global/node_modules/@mariozechner/pi-coding-agent/dist/modes/interactive/theme/theme.js
```

### Set up tmux for side-by-side comparison
```bash
tmux new-session -d -s tui-compare -x 120 -y 40
tmux split-window -h -t tui-compare
# Left pane: pi TUI, Right pane: your Rust TUI (later)
```

### Use a cheap model for all tmux testing

When running pi in tmux to capture its visual output, **always use a cheap model**. The content of the responses doesn't matter — you only need to observe how the TUI renders messages, tool calls, streaming, etc. Use:

```bash
pi --model claude-3-5-haiku-latest
```

This avoids burning expensive tokens on visual capture sessions. Every `tmux send-keys` invocation that launches pi in this prompt should use `--model claude-3-5-haiku-latest`.

---

## Phase 1: Build the TUI Engine

### 1.1 Drop ratatui

Remove `ratatui` from `crates/coop-tui/Cargo.toml` and the workspace `Cargo.toml`. Replace with a direct dependency on `crossterm`. The new engine will be ~300 lines of Rust mirroring `tui.js`.

### 1.2 Core trait — direct translation of pi's Component interface

```rust
/// A styled line of text containing ANSI escape sequences.
/// Each line must not exceed the width passed to render().
pub type StyledLine = String;

/// The pi Component interface, translated to Rust.
pub trait Component {
    /// Render the component at the given width.
    /// Returns lines of ANSI-styled text, each ≤ width visible characters.
    fn render(&self, width: usize) -> Vec<StyledLine>;

    /// Handle keyboard input. Returns true if the input was consumed.
    fn handle_input(&mut self, _data: &[u8]) -> bool { false }

    /// Clear cached render state (called on theme changes, resize).
    fn invalidate(&mut self) {}
}
```

### 1.3 Container — identical to pi's Container

```rust
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        self.children.iter().flat_map(|c| c.render(width)).collect()
    }
    fn invalidate(&mut self) {
        for child in &mut self.children { child.invalidate(); }
    }
}
```

### 1.4 TUI renderer — translation of tui.js doRender()

The differential renderer must:
1. Call `self.render(width)` to get all lines
2. Compare against `previous_lines`
3. Find `first_changed` and `last_changed` indices
4. Move cursor to `first_changed` using `CSI nA` / `CSI nB`
5. Clear and rewrite only changed lines (`CSI 2K` + line content)
6. Wrap output in synchronized update markers (`CSI ?2026h` / `CSI ?2026l`)
7. Track `cursor_row`, `hardware_cursor_row`, `max_lines_rendered` exactly as pi does
8. Handle content shrinking (clear extra lines)
9. Handle width changes (full re-render with scrollback clear)

### 1.5 Scrollback and resize — the hardest parts to get right

These behaviors are where TUIs break. Study `tui.js doRender()` carefully for how pi handles them:

**Scrollback behavior (how content grows beyond the terminal):**
- Pi tracks `maxLinesRendered` — the high-water mark of lines ever rendered. This only grows (never shrinks unless a full clear happens).
- `viewportTop = max(0, maxLinesRendered - terminalHeight)` determines which lines are visible.
- When new content is appended that pushes `maxLinesRendered` past the terminal height, pi emits `\r\n` sequences to scroll the terminal. The lines that scroll off the top become native terminal scrollback.
- The `previousViewportTop` tracks where the viewport was last frame, so cursor movement can be calculated correctly even after scrolling.
- When content *shrinks* (e.g., a component renders fewer lines), pi handles this via `clearOnShrink` — optionally clearing the extra lines. With overlays active, shrinking is suppressed because overlay positioning depends on stable line indices.
- **The user can scroll up** in their terminal at any time to see messages that have scrolled into scrollback. This is a fundamental feature — long conversations remain accessible.

**Verify scrollback works by testing:**
1. Send enough messages that content exceeds 40 lines (the terminal height in your tmux test)
2. Verify older messages are in terminal scrollback (scroll up with mouse wheel or `tmux copy-mode`)
3. Verify the editor + footer remain pinned at the bottom
4. Verify new messages appear correctly and old ones don't get corrupted
5. Capture the scrollback with `tmux capture-pane -t picap -p -S -100` (captures 100 lines of scrollback above visible area)

**Resize behavior (what happens when the terminal changes size):**
- Pi detects width changes by comparing `previousWidth` to current `terminal.columns`.
- On width change: **full re-render with scrollback clear** (`\x1b[3J\x1b[2J\x1b[H` — clear scrollback, clear screen, cursor home). This is necessary because line wrapping changes invalidate all previous output.
- On height change without width change: the viewport calculation adjusts automatically since `viewportTop = max(0, maxLinesRendered - terminalHeight)`. No explicit clear is needed.
- After a full clear, `maxLinesRendered` resets to the new content length.
- All components get `invalidate()` called on resize so cached renders are discarded.

**Verify resize works by testing:**
1. Launch the TUI in a tmux pane at 120x40
2. Send a few messages so there's content
3. Resize the pane: `tmux resize-pane -t picap -x 80 -y 30`
4. Verify the content reflows correctly (no corruption, no orphaned lines)
5. Resize back: `tmux resize-pane -t picap -x 120 -y 40`
6. Verify it recovers cleanly
7. Resize during streaming (while agent is responding) — verify no visual corruption
8. Compare captures before and after resize at the same size — content should match

**Translate these mechanisms exactly from `tui.js`.** The variables `cursorRow`, `hardwareCursorRow`, `maxLinesRendered`, `previousViewportTop`, `previousWidth`, `previousLines` all exist for a reason. Port them all.

### 1.6 Utility functions — translation of utils.js

Implement:
- `visible_width(s: &str) -> usize` — display width ignoring ANSI escapes (use `unicode_width`)
- `truncate_to_width(s: &str, width: usize) -> String` — ANSI-aware truncation
- `wrap_text_with_ansi(s: &str, width: usize) -> Vec<String>` — word wrap preserving ANSI
- `fg(r, g, b, text: &str) -> String` — wrap text in 24-bit foreground color
- `bg(r, g, b, text: &str) -> String` — wrap text in 24-bit background color
- `bold(text: &str) -> String`, `italic(text: &str) -> String`

### 1.7 Basic components — direct translations

- **Text**: Multi-line text with word wrapping and optional background. (`text.js`)
- **Box**: Container with padding and background color function. (`box.js`)
- **Spacer**: N empty lines. (`spacer.js`)
- **Markdown**: Markdown renderer with syntax highlighting. (`markdown.js` — use `pulldown-cmark` + `syntect`)

---

## Phase 2: Capture the Pi TUI for Reference

### 2.1 Launch and capture initial state
```bash
tmux new-session -d -s picap -x 120 -y 40
tmux send-keys -t picap 'pi --model claude-3-5-haiku-latest' Enter
sleep 3
tmux capture-pane -t picap -p > /tmp/pi-initial.txt
tmux capture-pane -t picap -p -e > /tmp/pi-initial-ansi.txt
```

### 2.2 Capture each state systematically

For every state below, capture both plain text (`-p`) and ANSI (`-p -e`) versions:

| State | How to trigger | Filename |
|-------|---------------|----------|
| Welcome/idle | Launch pi, wait | `idle` |
| Typing | Type "Hello world" without submitting | `typing` |
| Multi-line input | Type, Shift+Enter, type more | `multiline` |
| User message | Submit "What is 2+2?" | `user-msg` |
| Streaming | Capture during response streaming | `streaming` |
| Complete response | Wait for response to finish | `response` |
| Tool pending | Submit "List files in docs/" — capture during execution | `tool-pending` |
| Tool complete | Wait for tool to finish | `tool-complete` |
| Thinking | Enable thinking with Shift+Tab, submit a question | `thinking` |
| Error | Trigger an error state | `error` |
| Scrollback | Send 5+ messages so content exceeds terminal | `scrollback` |
| Scrollback-history | `tmux capture-pane -S -200` to get scrolled-off content | `scrollback-history` |
| Post-resize-narrow | Resize to 80x30, capture | `resize-narrow` |
| Post-resize-restore | Resize back to 120x40, capture | `resize-restore` |

Save all captures to `crates/coop-tui/tests/captures/`.

For scrollback captures, use `tmux capture-pane -S -200 -p` to capture lines that have scrolled off the top of the visible pane. This verifies that older messages are preserved in terminal scrollback.

### 2.3 Extract key measurements from captures

From the ANSI captures, document:
- Border character: `─` (U+2500) colored with `38;2;129;162;190` = `#81a2be`
- Cursor rendering: `\x1b[7m \x1b[0m` (inverse video space at end, inverse video character inline)
- User message: entire block wrapped in `\x1b[48;2;52;53;65m` = `#343541`
- Tool success box: `\x1b[48;2;40;50;40m` = `#283228`
- Tool command: bold `$ ls docs/`
- Tool output: `38;2;128;128;128` = `#808080`
- Collapse hint: `... (7 earlier lines, ctrl+o to expand)`
- Footer dim text: `38;2;102;102;102` = `#666666`
- Thinking text: `\x1b[3m\x1b[38;2;128;128;128m` = italic gray

---

## Phase 3: Build Components (Test-Driven)

### 3.1 Testing approach

Since the engine is line-based, tests are simple string comparisons:

```rust
#[test]
fn editor_renders_empty_state() {
    let editor = Editor::new(/* theme */);
    let lines = editor.render(80);
    // Top border
    assert_eq!(lines[0], "\x1b[38;2;129;162;190m" + &"─".repeat(80) + "\x1b[0m");
    // Empty content with cursor
    assert_eq!(lines[1], "\x1b[7m \x1b[0m" + &" ".repeat(79));
    // Bottom border
    assert_eq!(lines[2], "\x1b[38;2;129;162;190m" + &"─".repeat(80) + "\x1b[0m");
}
```

For each component, write tests first based on the tmux captures, then implement until they pass.

### 3.2 Component build order

1. **Utility functions** (visible_width, truncate, wrap, color helpers) — test with known ANSI strings from captures
2. **Theme** — define all color constants from `dark.json`, implement `fg()`, `bg()`, `bold()`, `italic()`
3. **Spacer** — trivial, returns N empty strings
4. **Text** — word-wrapped styled text with optional background
5. **Box** — padded container with background
6. **Editor** — the input component (borders, cursor, multi-line, scroll indicators). This is the most complex component (~1700 lines in pi). Translate `editor.js` section by section.
7. **Footer** — 2-line status bar (pwd + stats + model name)
8. **UserMessage** — markdown with `#343541` background
9. **AssistantMessage** — markdown, thinking blocks
10. **ToolExecution** — pending/success/error states, collapsible output
11. **Markdown** — full renderer with syntax highlighting
12. **TUI engine** — differential renderer, input dispatch, overlay compositing

### 3.3 Test each component against captures

After building each component, compare its `render()` output against the corresponding captured ANSI lines:

```rust
#[test]
fn footer_matches_pi_capture() {
    let footer = Footer::new(/* ... */);
    let lines = footer.render(120);
    // From /tmp/pi-initial-ansi.txt, the footer lines are:
    assert_eq!(lines[0], "\x1b[38;2;102;102;102m~/coop/coop (main)\x1b[39m");
    assert!(lines[1].starts_with("\x1b[38;2;102;102;102m"));
    assert!(lines[1].contains("claude-opus-4-6 • medium"));
}
```

---

## Phase 4: Integration with Coop Gateway

### 4.1 Update `crates/coop-gateway/src/main.rs`

Replace the ratatui-based event loop with the new engine:

```rust
// Old: ratatui Terminal with inline viewport + insert_before
// New: TUI engine with differential rendering (like pi)

let mut tui = Tui::new(/* terminal */);

// Add components in layout order
tui.add_child(header);
tui.add_child(chat_container);
tui.add_child(status_container);
tui.add_child(editor);
tui.add_child(footer);

tui.set_focus(&editor);
tui.start();

// Event loop
loop {
    // Receive TurnEvents from gateway
    while let Ok(event) = event_rx.try_recv() {
        match event {
            TurnEvent::TextDelta(text) => {
                assistant_msg.append_text(&text);
                tui.request_render();
            }
            TurnEvent::ToolStart { name, args, .. } => {
                let tool = ToolExecution::new(&name, &args);
                chat_container.add_child(tool);
                tui.request_render();
            }
            // ... etc
        }
    }

    // Poll for keyboard input
    if let Some(data) = poll_input(Duration::from_millis(16)) {
        tui.handle_input(&data);
    }
}
```

### 4.2 Preserve the gateway contract

The TUI must still:
- Use `tokio::sync::mpsc` to receive `TurnEvent` from `Gateway::run_turn`
- Handle `TurnEvent::TextDelta`, `ToolStart`, `ToolResult`, `Done`, `Error`
- Submit user input via `Gateway::run_turn` in a spawned tokio task
- Track tool names by ID for correlating results

---

## Phase 5: Side-by-Side Verification

### 5.1 Automated comparison
```bash
#!/bin/bash
set -euo pipefail

# Launch both in fixed-size tmux (use cheap model for pi — we only care about TUI visuals)
tmux new-session -d -s verify -x 120 -y 40
tmux send-keys -t verify 'pi --model claude-3-5-haiku-latest' Enter
sleep 3

tmux split-window -h -t verify
tmux send-keys -t verify 'cargo run -- chat' Enter  
sleep 3

# Capture idle state
tmux capture-pane -t verify:0.0 -p > /tmp/pi.txt
tmux capture-pane -t verify:0.1 -p > /tmp/coop.txt

# Compare (strip volatile: timestamps, spinner frames, token counts)
sed -E 's/[0-9]+\.[0-9]+%/X.X%/g; s/\$[0-9.]+/\$X/g' /tmp/pi.txt > /tmp/pi-stable.txt
sed -E 's/[0-9]+\.[0-9]+%/X.X%/g; s/\$[0-9.]+/\$X/g' /tmp/coop.txt > /tmp/coop-stable.txt

diff /tmp/pi-stable.txt /tmp/coop-stable.txt && echo "MATCH" || echo "MISMATCH"
```

### 5.2 States to verify
Run both TUIs through identical input sequences and diff at each step:
1. Idle/welcome
2. Type text in editor
3. Submit and receive response
4. Tool call lifecycle (pending → complete)
5. Multi-line input
6. Thinking blocks (if model supports it)

### 5.3 Scrollback verification (critical)

This is the most important behavioral test. A TUI that looks right but breaks scrollback is unusable.

```bash
#!/bin/bash
# Test scrollback behavior in both TUIs
for pane in 0.0 0.1; do
  # Send multiple messages to push content beyond terminal height
  for i in 1 2 3 4 5; do
    tmux send-keys -t verify:$pane "Message $i: tell me a short joke" Enter
    sleep 8  # wait for response
  done
  
  # Capture visible area
  tmux capture-pane -t verify:$pane -p > /tmp/verify-${pane}-visible.txt
  
  # Capture scrollback (200 lines above visible)
  tmux capture-pane -t verify:$pane -S -200 -p > /tmp/verify-${pane}-scrollback.txt
  
  # Verify editor + footer are at bottom of visible area
  tail -5 /tmp/verify-${pane}-visible.txt
  
  # Verify early messages exist in scrollback
  grep -c "Message 1" /tmp/verify-${pane}-scrollback.txt
done

# Compare scrollback structure
diff /tmp/verify-0.0-scrollback.txt /tmp/verify-0.1-scrollback.txt
```

**What to check:**
- Early messages (Message 1, 2) should appear in scrollback capture, not visible area
- The editor borders (`────`) and footer should always be in the visible area
- No garbled or duplicated lines in scrollback
- Scrollback content should be identical between pi and coop (minus volatile data)

### 5.4 Resize verification (critical)

```bash
#!/bin/bash
# Test resize behavior — must not corrupt display
for pane in 0.0 0.1; do
  # Send a couple messages first
  tmux send-keys -t verify:$pane "What is 2+2?" Enter
  sleep 5
  
  # Capture at original size (120x40)
  tmux capture-pane -t verify:$pane -p > /tmp/verify-${pane}-before.txt
done

# Resize both panes to narrow
tmux resize-pane -t verify:0.0 -x 60 -y 25
tmux resize-pane -t verify:0.1 -x 60 -y 25
sleep 1

for pane in 0.0 0.1; do
  tmux capture-pane -t verify:$pane -p > /tmp/verify-${pane}-narrow.txt
done

# Resize back to original
tmux resize-pane -t verify:0.0 -x 120 -y 40
tmux resize-pane -t verify:0.1 -x 120 -y 40
sleep 1

for pane in 0.0 0.1; do
  tmux capture-pane -t verify:$pane -p > /tmp/verify-${pane}-restored.txt
done

# Compare narrow captures
diff /tmp/verify-0.0-narrow.txt /tmp/verify-0.1-narrow.txt

# Compare restored captures
diff /tmp/verify-0.0-restored.txt /tmp/verify-0.1-restored.txt

# Check for corruption: no line should exceed pane width
awk -v w=60 '{if(length > w) print NR": "length" > "w": "$0}' /tmp/verify-0.0-narrow.txt
awk -v w=60 '{if(length > w) print NR": "length" > "w": "$0}' /tmp/verify-0.1-narrow.txt
```

**What to check:**
- After narrowing: content reflows, no lines exceed the new width, editor/footer still visible
- After restoring: display recovers cleanly, no orphaned lines or corruption
- The narrow capture should show the same content as the wide capture, just reformatted
- Scrollback is cleared on width change (pi clears scrollback with `\x1b[3J` on resize)

### 5.5 ANSI-level comparison

```bash
tmux capture-pane -t verify:0.0 -p -e > /tmp/pi-ansi.txt
tmux capture-pane -t verify:0.1 -p -e > /tmp/coop-ansi.txt
diff /tmp/pi-ansi.txt /tmp/coop-ansi.txt
```

This catches color mismatches invisible in plain-text diffs.

---

## Critical Rules

1. **Follow pi's architecture, do not redesign.** This is a structural port. Pi has a `Container` with `addChild` — you have a `Container` with `add_child`. Pi has `doRender()` with `previousLines`, `maxLinesRendered`, `hardwareCursorRow` — you have `do_render()` with `previous_lines`, `max_lines_rendered`, `hardware_cursor_row`. When you're unsure how to implement something, read the pi source and translate the logic. The Rust code should be recognizably the same architecture as the JavaScript. Redesigning means introducing behavioral differences that are hard to debug.

2. **The engine is line-based, not cell-based.** Components return `Vec<String>` of ANSI-styled lines. There is no 2D buffer. There is no `Rect`. This is the fundamental architectural decision.

3. **Do not use ratatui.** Remove it from dependencies. Use `crossterm` directly for terminal control (raw mode, cursor movement, size queries). The rendering is raw ANSI string writes.

4. **Do not guess colors.** Use the exact hex values from `dark.json`. Cross-reference with the ANSI captures (`38;2;r;g;b` sequences) to verify.

5. **Do not skip the differential renderer.** Pi's TUI only rewrites changed lines. A full-repaint engine will flicker. Implement the `first_changed`/`last_changed` diffing and synchronized output from `tui.js doRender()`.

6. **Scrollback must work.** Content that grows beyond the terminal height must scroll into native terminal scrollback. The user must be able to scroll up to see old messages. This is not optional — it's the primary way users read long conversations. Test this explicitly with `tmux capture-pane -S -200`. If scrollback is broken, the TUI is broken.

7. **Resize must work.** Width changes must trigger a full re-render with scrollback clear (`\x1b[3J\x1b[2J\x1b[H`). Height changes must adjust the viewport. No corruption, no orphaned lines, no lines exceeding the new width. Test resize during idle AND during streaming. If resize is broken, the TUI is broken.

8. **Do not break the gateway integration.** The TUI connects to `crates/coop-gateway` via `TurnEvent` channels. Update `main.rs` to use the new engine but keep the same event flow.

9. **Test components as `Vec<String>` assertions.** This is the payoff of the line-based architecture — every component's output is directly testable as string comparisons against captured pi output.

10. **Follow all coop project rules** from `AGENTS.md` — `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, no PII.

11. **The terminal is your truth.** What `tmux capture-pane -p -e` shows is the spec. Match it exactly.
