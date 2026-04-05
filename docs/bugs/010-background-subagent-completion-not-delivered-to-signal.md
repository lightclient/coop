# BUG-010: Background subagent completion was stored in-session but not delivered to Signal

**Status:** Fixed
**Found:** 2026-04-05
**Scenario:** Live Signal e2e verification of background subagent runs

## Symptom

A background subagent run completed successfully, but the Signal user never received the `[subagent completion]` notification.

Observed behavior before the fix:
- the background subagent ran and completed
- the completion note was appended to the parent session history
- but no outbound Signal message was sent when the parent session was idle

This meant background completion worked only as hidden session state, not as a real Signal notification.

## Trace Evidence

Before the fix, e2e verification failed at the user-visible step:

```text
FAIL: background completion injection missing
```

The runtime logic only did one of the following:
- inject pending inbound if the parent turn was still active, or
- append a user message to the parent session if idle

There was no direct channel delivery for idle Signal parent sessions.

## Root Cause

`crates/coop-gateway/src/subagents/runtime.rs` handled background completion by updating session state only.

When the parent turn was idle, `announce_completion` called `gateway.append_message(...)`, which preserved the completion in session history but never routed it to the Signal channel.

## Fix

Fixed by wiring background completion delivery into the existing outbound delivery path:

- `crates/coop-gateway/src/subagents/runtime.rs`
  - added a bound outbound delivery sender to `SubagentManager`
  - when a background subagent completes and the parent session is idle, the completion note is now:
    1. appended to the parent session history, and
    2. delivered to the Signal DM/group target as an outbound message
- `crates/coop-gateway/src/main.rs`
  - bound the signal delivery bridge into the subagent manager in daemon startup
- `crates/coop-gateway/src/scheduler.rs`
  - exposed the underlying outbound channel sender so subagents can reuse the existing delivery bridge

## Test Coverage

Added unit coverage:

- `subagents::runtime::tests::background_mode_delivers_completion_to_signal_parent_when_idle`

Verified end-to-end over live Signal by sending a background subagent request and confirming the sender received a message containing:

```text
[subagent completion]
status=completed
```