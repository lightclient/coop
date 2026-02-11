# Concurrent Multi-User Signal Loop

## Task

Make the Signal loop support concurrent multi-user turns so no user blocks
another while waiting for their response.

## Problem

The signal loop in `crates/coop-gateway/src/signal_loop.rs` serializes ALL
incoming messages behind a single `active_turn: Option<JoinHandle>`. When
Alice's turn is running, Bob's incoming Signal message blocks at
`active_turn.take().await` until Alice's entire multi-iteration turn
completes — even though they use completely separate sessions (different
`SessionKind::Dm` keys).

```rust
// Current — global serialization point
let mut active_turn: Option<JoinHandle<Result<()>>> = None;
loop {
    let inbound = Channel::recv(&mut signal_channel).await?;
    // ...
    // ⚠️ BLOCKS HERE — waits for ANY previous turn regardless of session
    if let Some(task) = active_turn.take()
        && let Err(error) = task.await
    {
        warn!(error = %error, "previous signal turn task failed");
    }
    active_turn = Some(tokio::spawn(...));
}
```

One `Option<JoinHandle>` for all users means the loop processes one turn at
a time globally.

## Architecture Context (what already works)

Before changing anything, understand what is already concurrent:

- **Gateway** (`gateway.rs`): Per-session `tokio::sync::Mutex` via
  `session_turn_locks` with `try_lock()`. Prevents concurrent turns from
  corrupting the same session's message history. Does NOT block across
  different sessions. ✅

- **IPC/Gateway** (`main.rs`): Each `coop attach` client gets its own
  `tokio::spawn(handle_client(...))`. Multiple attached TUI clients already
  run in parallel. ✅

- **Scheduler** (`scheduler.rs`): Cron and reminder fires already
  `tokio::spawn(fire_cron(...))` / `tokio::spawn(fire_reminder(...))`
  independently. ✅

- **Typing indicators**: `TypingGuard` is created per-turn inside
  `run_turn_with_trust()`, targets the specific `SessionKey`.
  `SignalTypingNotifier` sends via the action channel. Already per-session. ✅

- **Routing** (`router.rs`): Stateless. Signal DMs route to
  `SessionKind::Dm(identity)` — each user gets their own session. ✅

The signal loop is the **only chokepoint**.

## Fix

Replace the single `active_turn: Option<JoinHandle>` with a per-session
`HashMap<SessionKey, JoinHandle>` so turns on different sessions run
concurrently.

## Changes (signal_loop.rs only)

In `run_signal_loop()`:

1. **Replace the single active turn tracker** with per-session tracking:

   ```rust
   // WAS:
   let mut active_turn: Option<JoinHandle<Result<()>>> = None;

   // BECOMES:
   let mut active_turns: HashMap<SessionKey, JoinHandle<Result<()>>> = HashMap::new();
   ```

   Add imports for `HashMap` from `std::collections` and `SessionKey` from
   `coop_core`.

2. **Remove the await-previous-turn block** entirely:

   ```rust
   // DELETE THIS BLOCK:
   if let Some(task) = active_turn.take()
       && let Err(error) = task.await
   {
       warn!(error = %error, "previous signal turn task failed");
   }
   ```

3. **Add cleanup and per-session check** before spawning:

   ```rust
   // Clean up completed turns (finished or panicked)
   active_turns.retain(|_, task| !task.is_finished());

   // Route to determine the target session
   let decision = router.route(&inbound);

   // If this session already has an active turn, skip.
   // The gateway's try_lock would reject it anyway, but we avoid the
   // spawn overhead and log a clearer message.
   if active_turns.contains_key(&decision.session_key) {
       debug!(
           session = %decision.session_key,
           "skipping message: session already has an active turn"
       );
       continue;
   }
   ```

4. **Track the spawned turn per-session** instead of globally:

   ```rust
   // WAS:
   active_turn = Some(tokio::spawn(async move { ... }));

   // BECOMES:
   let session_key = decision.session_key.clone();
   active_turns.insert(session_key, tokio::spawn(async move { ... }));
   ```

5. **Reuse the route decision** for history bootstrap. The code currently
   calls `router.route(&inbound)` for the bootstrap check. Since we already
   computed the decision in step 3, reuse it instead of calling route again.
   Move the bootstrap block after the per-session check so it uses the
   existing `decision`.

## What NOT to change

- `gateway.rs` — per-session turn locks already handle same-session conflicts
  correctly via `try_lock()`.
- `router.rs` — routing logic is stateless and correct.
- `scheduler.rs` — already spawns cron/reminder tasks independently.
- `main.rs` — IPC client handling is already concurrent (one task per
  accepted connection). Within a connection, sequential is correct (TUI can
  only send one message at a time).
- Typing indicators — already per-session via `TypingGuard` created in
  `run_turn_with_trust()`. Each user's DM session gets its own guard.
- IPC protocol — no protocol changes needed.
- `coop-core` types — no changes.

## Test updates (signal_loop/tests.rs)

Existing tests use `handle_signal_inbound_once()`, a single-message test
helper that doesn't go through `run_signal_loop()`. These tests should pass
unchanged.

Add a new test that verifies concurrent multi-user behavior:

- Create a provider with a configurable delay (sleep 200ms per call).
- Queue two Signal messages from different senders (different DM sessions).
- Run both through the signal loop (or simulate the dispatch path).
- Assert both complete within ~1x the delay (not 2x), proving concurrency.
- Assert each user's session has the correct messages.
- Assert both users received typing indicators independently.

## Verification checklist

1. `cargo build` — compiles.
2. `cargo test -p coop-gateway` — all existing tests pass.
3. `cargo clippy --all-targets --all-features -- -D warnings` — clean.
4. `cargo fmt` — formatted.
5. Conceptual verification: Alice and Bob message on Signal simultaneously →
   both get typing indicators immediately, both get responses in ~parallel
   time, neither blocks the other. Cron jobs and reminders continue firing
   unaffected during active user turns.
