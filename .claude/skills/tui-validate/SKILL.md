---
name: tui-validate
description: Validates the Coop TUI by capturing output from a tmux session using freeze and applying LLM-as-judge semantic validation. Supports text and visual (PNG/SVG) validation modes.
type: anthropic-skill
version: "1.0"
---

# TUI Validate

## Overview

This skill validates the Coop TUI by launching it in a tmux session, sending input, capturing the rendered output with [freeze](https://github.com/charmbracelet/freeze), and using LLM-as-judge for semantic validation.

**Philosophy**: Rather than brittle string matching, this skill uses semantic understanding to validate that TUI output "looks right" — checking layout, content presence, and visual hierarchy without breaking on minor formatting changes.

## When to Use

- After making changes to TUI rendering, gateway, or provider code
- To verify the full end-to-end flow: user input → provider call → response display
- To check tool call activity is visible in the TUI
- Visual regression testing for the terminal interface
- Verifying error handling displays correctly

## Prerequisites

**Required:**
- `freeze` CLI tool installed (charmbracelet/freeze)
- `tmux` for interactive TUI capture
- The `coop` binary built (`cargo build --release`)

**Verification:**
```bash
freeze --help | head -1
tmux -V
ls target/release/coop
```

## Parameters

- **target** (required): What to validate. One of:
  - `tmux:<session>` — Capture a live tmux session pane
  - `command:<cmd>` — Execute a command and capture output
  - `buffer:<text>` — Raw text/ANSI to validate directly

- **criteria** (required): Validation criteria. Can be:
  - A predefined criteria name (see Built-in Criteria below)
  - A custom criteria string describing what to check

- **output_format** (optional, default: `"svg"`): Screenshot format
  - `svg` — Vector format, good for documentation
  - `png` — Raster format, good for visual diff
  - `text` — Text-only extraction, fastest

- **save_screenshot** (optional, default: `false`): Save screenshot to working directory

- **judge_mode** (optional, default: `"semantic"`): Validation approach
  - `semantic` — LLM judges based on meaning and layout
  - `strict` — Also checks exact content presence
  - `visual` — Requires PNG, checks visual appearance

## Built-in Criteria

### `coop-connected`
Validates Coop TUI launched and connected successfully:
- Shows "Connected to {agent_id}" system message
- Shows model name in connection message
- Input area is visible at the bottom
- No error messages present

### `coop-response`
Validates Coop TUI received an assistant response:
- User message is displayed
- Assistant response is present and non-empty
- No "Error:" system messages
- Loading indicator is gone (response complete)

### `coop-tool-activity`
Validates tool call activity is visible:
- Shows "[calling tool: ...]" or "[executing: ...]" system messages
- Assistant response follows tool activity
- No "Error:" system messages

### `coop-full`
Complete Coop TUI layout validation:
- Header/title bar is rendered
- Message area shows conversation
- Input area at bottom is visible
- Proper visual borders and hierarchy

### `tui-basic`
Generic TUI validation:
- Has visible content (not blank)
- No rendering artifacts or broken characters
- Proper terminal dimensions utilized

## Execution Flow

### 1. Setup Phase (for live TUI testing)

Launch Coop in a tmux session:

```bash
# Kill any existing session
tmux kill-session -t coop-test 2>/dev/null || true

# Start a new detached session running coop
tmux new-session -d -s coop-test -x 120 -y 40 'target/release/coop chat'

# Wait for TUI to initialize
sleep 2
```

### 2. Interaction Phase

Send input to the TUI via tmux:

```bash
# Type a message
tmux send-keys -t coop-test "say hello" Enter

# Wait for response (API call + rendering)
sleep 15
```

### 3. Capture Phase

Capture TUI output based on target type:

**For tmux targets:**
```bash
# Capture pane text content
tmux capture-pane -t coop-test -p > /tmp/coop-tui-capture.txt

# Generate visual screenshot
tmux capture-pane -t coop-test -p | freeze -o /tmp/coop-tui-capture.svg --theme base16 --width 120
```

**For command targets:**
```bash
freeze --execute "{command}" -o /tmp/coop-tui-capture.{format} --theme base16
```

**For buffer targets:**
```bash
echo "{buffer}" | freeze -o /tmp/coop-tui-capture.{format} --theme base16
```

**Constraints:**
- You MUST verify `freeze` is installed before attempting capture
- You MUST handle capture failures gracefully and report the error
- You SHOULD use `--theme base16` for consistent rendering
- You SHOULD set reasonable dimensions with `--width 120`

### 4. Extraction Phase

Extract content for LLM analysis:

- If format is `text`, use the captured text directly
- If format is `svg` or `png`, also capture a text version for content analysis
- For visual validation, analyze the image directly using vision capabilities

### 5. Validation Phase

Apply LLM-as-judge with the appropriate criteria:

**Semantic Validation:**
```
Analyze this terminal UI output and determine if it meets the following criteria:

CRITERIA:
{criteria_description}

TERMINAL OUTPUT:
{captured_text}

Evaluate each criterion and provide:
1. PASS or FAIL for each requirement
2. Brief explanation for any failures
3. Overall verdict: PASS or FAIL

Be lenient on exact formatting/whitespace but strict on:
- Required content presence
- Logical layout and hierarchy
- No rendering errors or artifacts
```

**Visual Validation (with PNG image):**
```
Examine this terminal screenshot and validate:

CRITERIA:
{criteria_description}

Check for:
1. Visual hierarchy and layout
2. Color coding correctness
3. No rendering artifacts or broken characters
4. Proper alignment and spacing

Verdict: PASS or FAIL with explanation
```

**Constraints:**
- You MUST return a clear PASS or FAIL verdict
- You MUST provide specific feedback on failures
- You MUST be lenient on whitespace/formatting differences
- You MUST be strict on content presence and semantic correctness
- You SHOULD note any warnings even on PASS results

### 6. Teardown Phase

Clean up the tmux session:

```bash
# Quit the TUI gracefully
tmux send-keys -t coop-test C-c
sleep 1

# Kill the session
tmux kill-session -t coop-test 2>/dev/null || true
```

### 7. Reporting Phase

Report validation results:

**On PASS:**
```
✅ TUI Validation PASSED

Criteria: {criteria_name}
Target: {target}
Mode: {judge_mode}

All requirements satisfied.
{optional_notes}
```

**On FAIL:**
```
❌ TUI Validation FAILED

Criteria: {criteria_name}
Target: {target}
Mode: {judge_mode}

Issues found:
- {issue_1}
- {issue_2}

Screenshot saved: {path_if_saved}
```

## Examples

### Example 1: Validate Coop Starts and Connects

```
/tui-validate target:tmux:coop-test criteria:coop-connected
```

**Process:**
1. Assume coop is already running in tmux session `coop-test`
2. Capture pane: `tmux capture-pane -t coop-test -p > /tmp/capture.txt`
3. Generate screenshot: `tmux capture-pane -t coop-test -p | freeze -o /tmp/capture.svg --theme base16 --width 120`
4. Apply `coop-connected` criteria via LLM judge
5. Report PASS/FAIL

### Example 2: Full End-to-End Chat Validation

```
/tui-validate target:tmux:coop-test criteria:coop-response
```

**Process:**
1. Capture pane text and screenshot
2. Apply `coop-response` criteria — check for user message and assistant reply
3. Verify no error messages present
4. Report result

### Example 3: Validate Tool Calling Flow

```
/tui-validate target:tmux:coop-test criteria:coop-tool-activity save_screenshot:true
```

**Process:**
1. Capture pane (after sending a message that triggers tools)
2. Check for tool activity indicators (`[calling tool: ...]`, `[executing: ...]`)
3. Save screenshot for debugging
4. Report result

### Example 4: Custom Criteria

```
/tui-validate target:tmux:coop-test criteria:"Shows the agent name 'reid' in the UI and displays at least one assistant response" judge_mode:strict
```

### Example 5: Quick Buffer Validation

```
/tui-validate target:buffer:"Connected to reid (claude-sonnet-4-20250514). Type a message or /quit to exit." criteria:coop-connected output_format:text
```

## Typical Full Workflow

A complete TUI validation session typically looks like:

```bash
# 1. Build
cargo build --release

# 2. Launch in tmux
tmux kill-session -t coop-test 2>/dev/null || true
tmux new-session -d -s coop-test -x 120 -y 40 'target/release/coop chat'
sleep 2

# 3. Validate startup
# /tui-validate target:tmux:coop-test criteria:coop-connected

# 4. Send a message
tmux send-keys -t coop-test "say hello" Enter
sleep 15

# 5. Validate response
# /tui-validate target:tmux:coop-test criteria:coop-response save_screenshot:true

# 6. Teardown
tmux send-keys -t coop-test C-c
sleep 1
tmux kill-session -t coop-test 2>/dev/null || true
```

## Criteria Definitions

### coop-connected (Full Definition)

```yaml
name: coop-connected
description: Coop TUI launched and connected
requirements:
  - name: connection_message
    description: Shows "Connected to" system message with agent name
    required: true

  - name: model_display
    description: Shows model name in connection info
    required: true

  - name: no_errors
    description: No "Error:" messages present
    required: true

  - name: input_area
    description: Input/prompt area visible at bottom
    required: true
```

### coop-response (Full Definition)

```yaml
name: coop-response
description: Coop TUI received an assistant response
requirements:
  - name: user_message
    description: The user's sent message is visible
    required: true

  - name: assistant_response
    description: An assistant response is present with content
    required: true

  - name: no_errors
    description: No "Error:" messages present
    required: true

  - name: not_loading
    description: Loading indicator is not showing (response complete)
    required: true
```

### coop-tool-activity (Full Definition)

```yaml
name: coop-tool-activity
description: Tool calling activity visible in TUI
requirements:
  - name: tool_indicator
    description: Shows tool call indicators like "[calling tool: ...]" or "[executing: ...]"
    required: true

  - name: assistant_response
    description: An assistant response follows tool activity
    required: true

  - name: no_errors
    description: No "Error:" messages present
    required: true
```

## Troubleshooting

### freeze not found

```bash
# Linux — download binary
curl -sSfL https://github.com/charmbracelet/freeze/releases/download/v0.1.6/freeze_0.1.6_Linux_x86_64.tar.gz \
  | tar xz --strip-components=1 -C /usr/local/bin freeze_0.1.6_Linux_x86_64/freeze
```

### tmux capture is empty

- Ensure the tmux session exists: `tmux list-sessions`
- Verify the TUI had time to render: increase sleep time
- Try: `tmux capture-pane -t coop-test -p` manually to check

### Coop fails to start in tmux

- Check `ANTHROPIC_API_KEY` is set in the tmux environment
- Verify `coop.yaml` exists in the working directory
- Check: `tmux send-keys -t coop-test "" ""` to verify session is alive

### LLM judge too strict/lenient

- Use `semantic` mode for layout/presence checking (default, most forgiving)
- Use `strict` mode when specific text must be present
- Use `visual` mode with PNG when appearance matters
