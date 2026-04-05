# BUG-009: Signal receive loop repeatedly fails with websocket upgrade errors

**Status:** Fixed
**Found:** 2026-04-05
**Scenario:** Live Signal e2e verification for subagents using an existing linked Signal DB

## Symptom

Coop started with the Signal channel enabled, but it never received inbound Signal messages.

Observed behavior before the fix:
- startup succeeded far enough to log `signal channel configured`, `building signal name resolver`, and `signal loop listening`
- repeated warnings appeared immediately after startup: `signal receive setup failed` with `Websocket error: websocket upgrade failed`
- sending `/status` from the other registered local Signal account succeeded at the `signal-cli send` level, but coop never logged inbound receipt or command handling
- `send-and-verify.sh` timed out waiting for the message to be processed

## Trace Evidence

Before the fix:

```text
INFO  signal channel configured
INFO  building signal name resolver self_aci=[redacted-uuid]
INFO  signal loop listening
WARN  signal receive setup failed error="libsignal-service error: Websocket error: websocket upgrade failed"
WARN  signal receive setup failed error="libsignal-service error: Websocket error: websocket upgrade failed"
WARN  signal receive setup failed error="libsignal-service error: Websocket error: websocket upgrade failed"
```

After fixing BUG-008 and relinking the secondary device, the Signal e2e flow succeeded:

```text
Results:
  ✅ Message received by coop
  ✅ Message dispatched
  ✅ Slash command handled
  ✅ No agent_turn for command
  ✅ No errors
```

## Root Cause

This turned out to be a consequence of BUG-008 rather than a separate receive-loop implementation bug.

The previously used linked-device database had been produced by a broken secondary-device provisioning flow. Once `coop signal link` was fixed and a fresh `db/signal.db` was created, the receive loop started working and inbound Signal messages reached coop normally.

## Fix

No dedicated receive-loop code change was required.

BUG-008's linked-device registration fix produced a valid fresh Signal DB, and that resolved the websocket-upgrade failure seen during receive setup.

## Test Coverage

Verified by live Signal e2e after relinking:

- `bash .claude/skills/signal-e2e-test/scripts/preflight.sh traces.jsonl`
- `bash .claude/skills/signal-e2e-test/scripts/send-and-verify.sh "/status" --trace-file traces.jsonl`
- successful end-to-end subagent Signal scenarios after the fresh link