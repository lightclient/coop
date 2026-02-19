# Queue messages received mid-turn instead of dropping them

## Bug

When a Signal user sends a message while their session already has an active agent turn, the message is silently dropped. The user receives a delivery receipt (Signal protocol level) so it appears received, but the agent never processes it and never responds.

**Trace evidence** (`bug.jsonl` at `2026-02-16T01:48:58.672469Z`):
```
"skipping message: session already has an active turn"
session: "reid:dm:signal:3255c505-cb0a-46bf-b25d-7134df11e2db"
```

The message "This second version is really hard to interact with" was dispatched by the signal loop, but discarded at the active-turn guard. The previous turn (started 40 seconds earlier) was still running tool calls. When that turn completed, the dropped message was gone forever — no response was ever sent.

## Current code

`crates/coop-gateway/src/signal_loop.rs`, `run_signal_loop()`:

```rust
let mut active_turns: HashMap<SessionKey, JoinHandle<Result<()>>> = HashMap::new();

loop {
    let inbound = Channel::recv(&mut signal_channel).await?;
    // ... filtering, command handling ...

    // Clean up completed turns
    active_turns.retain(|_, task| !task.is_finished());

    let decision = router.route(&inbound);

    // BUG: message is silently discarded
    if active_turns.contains_key(&decision.session_key) {
        tracing::debug!(
            session = %decision.session_key,
            "skipping message: session already has an active turn"
        );
        continue;
    }

    // ... bootstrap, spawn turn ...
}
```

There is also a redundant `try_lock` guard in `gateway.rs` `run_turn_with_trust()` (line ~362) that would independently reject the turn if the signal loop didn't catch it first. That guard should remain (defense in depth), but the signal loop should never reach it for queued messages because it should only dispatch them after the active turn completes.

## Required change

Add a per-session pending message queue. When a message arrives for a session that already has an active turn, buffer it. After the active turn completes, dispatch the pending message as a new turn.

### Data structure

Add alongside `active_turns`:

```rust
let mut active_turns: HashMap<SessionKey, JoinHandle<Result<()>>> = HashMap::new();
let mut pending_messages: HashMap<SessionKey, (InboundMessage, String)> = HashMap::new();
```

Each entry stores `(inbound_message, reply_target)`. Only the **most recent** pending message per session needs to be kept — if the user sends 3 messages while a turn is running, the agent should see all of them but only needs to start one new turn. The earlier messages from the same user will be in the session history (they were appended by the router as user messages even though no turn ran for them).

Wait — actually the messages are NOT appended to session history when they're dropped. They're discarded before `router.dispatch()` is called. So we need to think about this differently.

### Design: keep only the last pending message, dispatch it as the next turn

When the user sends multiple messages mid-turn, keep replacing the pending entry with the latest one. When the turn completes and the pending message is dispatched as a new turn, the agent will see it in context. The earlier mid-turn messages won't be in session history, but that's acceptable — the user's latest message supersedes their earlier ones (this matches how chat apps work: the latest message is what they want addressed).

If you want all mid-turn messages to be visible to the agent, an alternative is to concatenate them. But keep-latest is simpler and matches user expectations in messaging. Start with keep-latest.

### Loop structure

```rust
let mut active_turns: HashMap<SessionKey, JoinHandle<Result<()>>> = HashMap::new();
let mut pending: HashMap<SessionKey, (InboundMessage, String)> = HashMap::new();

loop {
    let inbound = Channel::recv(&mut signal_channel).await?;
    // ... filtering, command handling (unchanged) ...

    active_turns.retain(|_, task| !task.is_finished());

    // Drain pending messages for sessions whose turns just finished
    let finished_keys: Vec<SessionKey> = pending
        .keys()
        .filter(|k| !active_turns.contains_key(k))
        .cloned()
        .collect();
    for key in finished_keys {
        if let Some((pending_msg, pending_target)) = pending.remove(&key) {
            // Spawn a new turn for the pending message
            // (same spawn logic as the normal path below)
            spawn_turn(&mut active_turns, &signal_channel, &router, &key, &pending_msg, &pending_target);
        }
    }

    let decision = router.route(&inbound);

    if active_turns.contains_key(&decision.session_key) {
        tracing::info!(
            session = %decision.session_key,
            "queuing message: session has an active turn"
        );
        pending.insert(decision.session_key, (inbound, target));
        continue;
    }

    // ... bootstrap, spawn turn (unchanged) ...
}
```

### Important: drain pending BEFORE processing the new inbound

The drain must happen after `active_turns.retain()` (which cleans up finished tasks) and before the active-turn check for the new inbound. This ensures:
1. Finished turns are detected
2. Their pending messages are dispatched immediately
3. The new inbound message is then processed normally (and if the pending message just started a new turn for the same session, the new inbound will itself be queued — correct behavior)

### Extract spawn logic

The turn-spawn block (bootstrap check + `tokio::spawn`) is currently inline. Extract it into a helper to avoid duplicating it for the pending drain path:

```rust
fn spawn_turn(
    active_turns: &mut HashMap<SessionKey, JoinHandle<Result<()>>>,
    signal_channel: &SignalChannel,
    router: &Arc<MessageRouter>,
    session_key: &SessionKey,
    inbound: &InboundMessage,
    target: &str,
) {
    let router_clone = Arc::clone(router);
    let inbound_clone = inbound.clone();
    let target_clone = target.to_owned();
    let action_tx = signal_channel.action_sender();
    active_turns.insert(
        session_key.clone(),
        tokio::spawn(async move {
            dispatch_signal_turn_background(
                &action_tx,
                router_clone.as_ref(),
                &inbound_clone,
                &target_clone,
            )
            .await
        }),
    );
}
```

Note: the bootstrap (history seeding) logic should stay in the main path only, not in the pending drain — a session that already had a turn doesn't need bootstrapping.

### Tracing

- Change the existing `"skipping message"` debug log to `"queuing message: session has an active turn"` at INFO level
- Add `"dispatching queued message"` at INFO level when draining pending
- Add `pending_count` field to the queuing log if useful

### Limit

Add a constant `MAX_PENDING_PER_SESSION: usize = 1`. This makes the keep-latest semantic explicit. If we later want to buffer multiple messages, change this constant and switch to a `VecDeque`.

### Edge case: session bootstrap for pending messages

When a pending message is dispatched after drain, the session already has history from the previous turn, so `router.session_is_empty()` will be false. No bootstrap needed — skip it for the pending path.

## Test

Add a test in `crates/coop-gateway/src/signal_loop/tests.rs`:

**`queued_message_is_dispatched_after_active_turn_completes`**

Use `SlowFakeProvider` (already exists in the test file) to create a turn that takes time. Send a second message for the same session while the first is running. Verify:
1. The first turn completes and sends its response
2. The second message triggers a new turn after the first completes
3. The second turn completes and sends its response
4. Both responses are received (2 `SendText` actions for the same target)
5. The gateway has 4 messages in session history (user1, assistant1, user2, assistant2)

**`latest_queued_message_wins_when_multiple_arrive_mid_turn`**

Send 3 messages while a turn is running. Verify only the last one triggers a turn after the first completes. Session history should have user1, assistant1, user3 (the latest), assistant2.

Wait — if we only keep the latest, the middle messages are lost entirely. This might actually be wrong. Think about it: if the user sends "fix the bug", then "actually wait", then "ok go ahead" — only "ok go ahead" would be processed, and "actually wait" would be lost. That's probably fine for a messaging context. But if they sent "fix the bug" then "also update the docs" — losing the second message would be bad.

Actually, the simplest correct approach: **concatenate all pending messages** into a single inbound, separated by newlines, just like how the agent receives multi-line Signal messages. This way nothing is lost.

Revised: `pending` stores a `Vec<(InboundMessage, String)>` per session. When draining, concatenate the content of all pending messages into a single `InboundMessage` (using the metadata from the last one — timestamp, sender, etc.) and dispatch that as one turn.

**Actually, keep it even simpler:** just keep the latest message. The earlier mid-turn messages are a natural consequence of async messaging — the user is typing while the agent is working. The agent's response to the latest message will implicitly cover the conversation flow. If this proves insufficient in practice, upgrade to concatenation later. Don't over-engineer the first version.

## Files to change

1. `crates/coop-gateway/src/signal_loop.rs` — the main change (queue + drain logic)
2. `crates/coop-gateway/src/signal_loop/tests.rs` — new test(s)

## Do not change

- `gateway.rs` `run_turn_with_trust()` — keep the `try_lock` guard as defense in depth
- `router.rs` — no changes needed
- `coop-core` — no changes needed
- `coop-channels` — no changes needed
