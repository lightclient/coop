# BUG-006: Signal direct-message replies emit `could not create sync message from a direct message`

**Status:** Fixed
**Found:** 2026-03-18
**Scenario:** Signal e2e verification of `/models`, `/model`, `/status`, and follow-up turns

## Symptom

Outbound Signal DM actions emitted `ERROR` trace entries like:

```text
could not create sync message from a direct message
```

The visible user action still succeeded — commands were handled and replies were sent — but the e2e verifier reported failure because it requires zero `ERROR` entries in the new trace segment.

Observed patterns before the fix:
- command replies: 2 errors per interaction (delivery + read receipt path)
- normal turns: 4+ errors (receipts, typing start/stop, and/or final reply path)

## Trace Evidence

Before the fix:

```text
2026-03-18T13:31:13Z ERROR signal_action_send:send_message
  message="could not create sync message from a direct message"
  recipient="[redacted-id]"

2026-03-18T13:31:13Z DEBUG signal_action_send
  signal.action="delivery_receipt"
  message="signal action sent"
```

After the fix, the same flows produce only debug-level trace noise for unsupported sync-transcript content:

```text
DEBUG sending multi-device sync message
DEBUG skipping direct sync message for content without sent-transcript support content="ReceiptMessage"
DEBUG signal action sent
```

## Root Cause

`vendor/libsignal-service-rs/src/sender.rs` tried to create sent-transcript sync messages for content types that do not have a sent-transcript representation, such as delivery receipts, read receipts, and typing messages.

The helper `create_multi_device_sent_transcript_content` correctly returned `None` for those content types, but the caller treated that as an `ERROR` even though the original Signal action had already been sent successfully and no user-visible failure occurred.

## Fix

Fixed in `vendor/libsignal-service-rs/src/sender.rs` by downgrading those cases from error-level logging to debug-level logging when the content type does not support a sent transcript.

Real failures to create sync messages for normal message/edit content still log as errors.

## Test Coverage

Verification performed with live Signal e2e:

- `bash .claude/skills/signal-e2e-test/scripts/send-and-verify.sh "/status" --trace-file traces.jsonl`
- result now passes with `✅ No errors`

Additional regression coverage comes from the Signal-enabled build/tests and the manual trace verification of receipt and typing send paths.