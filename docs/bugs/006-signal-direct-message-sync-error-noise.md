# BUG-006: Signal direct-message replies emit `could not create sync message from a direct message`

**Status:** Open
**Found:** 2026-03-18
**Scenario:** Signal e2e verification of `/models`, `/model`, `/status`, and follow-up turns

## Symptom

Every outbound Signal DM action in this environment emits one or more `ERROR` trace entries with:

```text
could not create sync message from a direct message
```

The visible user action still succeeds — commands are handled and replies are sent — but the e2e verifier reports failure because it requires zero `ERROR` entries in the new trace segment.

Observed patterns:

- command replies: 2 errors per interaction (delivery + read receipt path)
- normal turns: 4+ errors (receipts, typing start/stop, and/or final reply path)

## Trace Evidence

```text
2026-03-18T13:31:13Z ERROR signal_action_send:send_message
  message="could not create sync message from a direct message"
  recipient="[redacted-id]"

2026-03-18T13:31:13Z DEBUG signal_action_send
  signal.action="delivery_receipt"
  message="signal action sent"

2026-03-18T13:31:13Z ERROR signal_action_send:send_message
  message="could not create sync message from a direct message"
  recipient="[redacted-id]"

2026-03-18T13:31:13Z DEBUG signal_action_send
  signal.action="read_receipt"
  message="signal action sent"
```

And during a normal turn:

```text
2026-03-18T13:38:03Z DEBUG codex_request message="Codex response complete"
2026-03-18T13:38:03Z ERROR signal_action_send:send_message
  message="could not create sync message from a direct message"
2026-03-18T13:38:03Z DEBUG signal_action_send
  signal.action="send_text"
  signal.raw_content="Four"
  message="signal action sent"
```

## Root Cause

Not root-caused in this session.

The failure appears to come from the Signal/presage/libsignal send path when it attempts to create a multi-device sync message for a direct message. The actual outward action still completes, so this looks like error-level trace noise or an avoidable sync-message path rather than a hard delivery failure.

## Fix

Not fixed in this session.

Likely follow-up areas:

1. Inspect the presage/libsignal direct-message send path used by `SignalAction::{SendText,Typing,...}`
2. Determine whether sync-message creation should be skipped for this target/session type
3. Downgrade known-benign failures if they do not affect user-visible delivery
4. Update the e2e verifier expectations only if the error is truly harmless and unavoidable

## Test Coverage

No regression test added yet.

A good follow-up test would assert that successful direct-message replies do not emit `ERROR` trace entries during receipt/typing/reply sends.
