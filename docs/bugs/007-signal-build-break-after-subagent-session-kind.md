# BUG-007: `cargo build --features signal` fails after adding `SessionKind::Subagent`

**Status:** Fixed
**Found:** 2026-04-05
**Scenario:** Signal e2e verification prep for subagent changes (`cargo build --features signal`)

## Symptom

The Signal-enabled build failed before e2e testing could start.

`cargo build --features signal` stopped in `coop-channels` with a non-exhaustive match on `SessionKind` after the new `Subagent` variant was introduced.

## Trace / Build Evidence

```text
error[E0004]: non-exhaustive patterns: `&SessionKind::Subagent(_)` not covered
   --> crates/coop-channels/src/signal.rs:138:28
    |
138 |         let target = match &session_key.kind {
    |                            ^^^^^^^^^^^^^^^^^ pattern `&SessionKind::Subagent(_)` not covered
```

## Root Cause

`SignalTypingNotifier::set_typing` matched every pre-existing `SessionKind` variant, but the new `SessionKind::Subagent(Uuid)` added by the subagent work was not handled.

That made the Signal feature set fail to compile even though the intended behavior for Signal typing on subagent sessions is to do nothing and return early.

## Fix

Updated `crates/coop-channels/src/signal.rs` so `SignalTypingNotifier::set_typing` treats `SessionKind::Subagent(_)` the same as non-Signal session kinds and returns without attempting to resolve a Signal target.

## Test Coverage

Verification performed with:

- `cargo fmt`
- `cargo build --features signal`
- `cargo clippy --all-targets --all-features -- -D warnings`

This bug is covered indirectly by the Signal-feature build and all-features clippy/test passes, which now include the `coop-channels` Signal code path.