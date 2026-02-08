# BUG-001: Empty assistant message sent to Anthropic API

**Status:** Fixed
**Found:** 2026-02-08
**Fixed:** 2026-02-08
**Trace:** `bug.jsonl` (last execution, starting at pid 84268)

## Symptom

Anthropic API returns 400 Bad Request:

```
messages.3: all messages must have non-empty content except for the optional final assistant message
```

## Root Cause

`format_messages()` in `crates/coop-agent/src/anthropic_provider.rs` filters out `Thinking` content blocks (`_ => None`), but still emits the message with `"content": []` — an empty array. Anthropic rejects any non-final message with empty content.

## Reproduction (from trace)

1. User sends "hello" → routed to agent turn
2. First provider request returns `stop_reason: "tool_use"` (signal_reply), `output_tokens: 173`
3. Tool executes, result appended. Second provider request returns `stop_reason: "end_turn"`, `output_tokens: 2` — response is a thinking block only, no visible text
4. In `SseState::handle_event` → `MessageStop`, thinking blocks are skipped and empty text is skipped, so `Message::assistant()` is constructed with **empty content**
5. Empty assistant message appended to session at `messages[3]`
6. User sends "who are you" → session history has 5 messages, `messages[3]` has `content: []`
7. Anthropic returns 400

## Fix (applied)

Three changes across two crates:

### 1. Drop empty messages in `format_messages()` (coop-agent)

`format_messages()` now skips any message whose content array is empty after filtering out thinking blocks. This prevents the 400 error from being sent in the first place.

### 2. Trust-gated error handling in `run_turn_with_trust()` (coop-gateway)

Provider errors are now caught inside `run_turn_with_trust()` instead of propagating as `Err(...)`. The behavior:

- **`TrustLevel::Full`**: `TurnEvent::Error` with the actual error message (e.g., API status + body)
- **Other trust levels**: `TurnEvent::Error` with generic "Something went wrong. Please try again later."
- Session history is rolled back to its pre-turn state (prevents corrupted history)
- `TurnEvent::Done` is always sent so consumers know the turn ended
- `run_turn_with_trust` returns `Ok(())` — callers no longer need to handle provider errors as panics/crashes
- Tracing logs the error with structured context: `error`, `iteration`, `trust`, `messages_rolled_back`

### 3. `dispatch_collect_text()` returns error as text (coop-gateway/router)

Previously converted `TurnEvent::Error` into `Err(...)`, which crashed the signal loop. Now collects the trust-gated error message as the response text, so the user gets a reply instead of silence.

## Test coverage

- `provider_error_sends_detail_to_full_trust_user` — Full trust sees real error, session rolled back
- `provider_error_hides_detail_from_public_trust_user` — Public trust gets generic message
- `provider_error_mid_turn_rolls_back_all_messages` — Error on iteration 1 (after tool execution) rolls back all accumulated messages
- `dispatch_collect_text_returns_error_as_text` — Full-trust signal user gets error detail as reply
- `dispatch_collect_text_returns_generic_error_for_public_user` — Public signal user gets generic reply
- `format_messages_drops_thinking_only_assistant_message` — Empty content arrays filtered (coop-agent)
