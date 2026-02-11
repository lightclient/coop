# Cron Direct Inject — Remove Announce LLM Call

## Problem

Cron delivery currently uses two LLM calls:

1. **Cron session** — isolated session evaluates the task (reads files, checks conditions, uses tools), produces output
2. **Announce to DM session** — injects cron output into the user's DM session, runs a *second* LLM turn that "summarizes the findings", delivers that second response

The second call is wasteful and harmful:

- It rephrases an already-composed message for no reason
- It can drop or misinterpret generative content (mood check-ins, questions)
- It strips the original task context, so when the user replies the DM agent lacks the "why"
- It doubles API cost and latency for every delivered cron job

## Solution

Eliminate the announce LLM call. After the cron session produces output, inject it directly into the DM session as messages (no LLM turn) and deliver to the channel.

### New flow

    cron fires
      -> cron session evaluates task (unchanged — has tools, reads files, etc.)
      -> agent produces output or HEARTBEAT_OK
      -> HEARTBEAT_OK -> suppress (unchanged)
      -> otherwise:
          1. append context user message to DM session (cron task + name)
          2. append cron output as assistant message to DM session
          3. deliver cron output to channel (Signal, etc.)
          -> no second LLM call

## Why inject both a user message and assistant message

When Alice replies to the cron message on Signal, her reply routes to the DM session. The DM agent needs context to handle it. By injecting both messages, the DM session history looks like:

    [... prior conversation with Alice ...]

    User: [Scheduled: humidifier-check]
    Read WEATHER.md for the current temperature. If below 30F, ask the user
    if they want the humidifier turned on. Otherwise HEARTBEAT_OK.

    Assistant: Hey! It's 22F out there this morning — pretty brutal. Want me
    to turn the humidifier on?

    User: yes please

Now when Alice says "yes please", the DM agent sees:
- The original cron instruction (what was the task about)
- The message it "sent" (what it asked Alice)
- Alice's reply

It has full context to act — turn on the humidifier, update a config file, whatever the appropriate action is.

Without the context user message, the DM agent would only see its own assistant message and Alice's reply, with no understanding of the underlying task.

## Concrete example walkthrough

### Config

    [[cron]]
    name = "humidifier-check"
    cron = "0 7 * * *"
    message = """Read WEATHER.md for the current temperature. \
    If it's below 30F, ask the user if they want the humidifier turned on. \
    Write your message as if texting them directly. \
    If not below 30, reply HEARTBEAT_OK."""
    user = "alice"

Alice has match = ["signal:alice-uuid"].

### Step 1: Cron session LLM call

Session key: coop:cron:humidifier-check

System prompt is built with channel="signal" (from delivery target), trust=Full, user="alice". Contains SOUL.md, BOOT.md, signal channel formatting instructions, tools, etc.

Messages sent to provider:

    System: [workspace prompt layers — SOUL.md, BOOT.md, channel context, runtime, etc.]

    User: [Your response will be delivered to the user via signal. Reply
    HEARTBEAT_OK if nothing needs attention. Do not use signal_send — your
    response is delivered automatically.]

    Read WEATHER.md for the current temperature. If it's below 30F, ask the
    user if they want the humidifier turned on. Write your message as if
    texting them directly. If not below 30, reply HEARTBEAT_OK.

Agent calls memory_get tool, reads WEATHER.md, sees 22F. Responds:

    Hey! It's 22F out there this morning — pretty brutal. Want me to turn
    the humidifier on?

### Step 2: HEARTBEAT_OK check

strip_heartbeat_token("Hey! It's 22F...") -> HeartbeatResult::Deliver(content)

Not suppressed. Proceed to injection.

### Step 3: Direct inject into DM session (NO LLM call)

Target DM session: coop:dm:signal:alice-uuid

Append two messages to this session:

Message 1 (User role, context):

    [Scheduled: humidifier-check]
    Read WEATHER.md for the current temperature. If it's below 30F, ask the
    user if they want the humidifier turned on. Write your message as if
    texting them directly. If not below 30, reply HEARTBEAT_OK.

Message 2 (Assistant role, the cron output):

    Hey! It's 22F out there this morning — pretty brutal. Want me to turn
    the humidifier on?

### Step 4: Deliver to Signal

Send the assistant message content to signal:alice-uuid via DeliverySender. Same delivery path as today.

### Step 5: Alice replies (later)

Alice texts back "yes please" on Signal. This routes to coop:dm:signal:alice-uuid as a normal inbound message. The DM session now has full context — the scheduled task instruction, the question asked, and Alice's reply. The agent processes the reply with its normal tools and conversation history.

### Contrast: suppressed case

If WEATHER.md shows 55F, the cron agent responds "HEARTBEAT_OK". strip_heartbeat_token catches it. Nothing is injected into the DM session. No delivery. No wasted second LLM call. Zero user-visible effect.

## Implementation

### Changes to scheduler.rs

Replace announce_to_session. Currently it calls router.inject_collect_text (which runs an LLM turn on the DM session). Replace with direct message appending.

#### Current announce_to_session (DELETE)

    async fn announce_to_session(
        cron_name: &str,
        cron_output: &str,
        channel: &str,
        target: &str,
        agent_id: &str,
        router: &MessageRouter,
        deliver_tx: Option<&DeliverySender>,
    ) {
        if target.starts_with("group:") {
            deliver_to_target(channel, target, cron_output, deliver_tx).await;
            return;
        }

        let announce_content = format!(
            "[Scheduled task \"{cron_name}\" produced the message below. ...]
            \n\n{cron_output}"
        );

        let injection = SessionInjection { ... };

        match router.inject_collect_text(&injection).await {  // <-- LLM CALL
            Ok((_decision, response)) => {
                deliver_to_target(channel, target, &response, deliver_tx).await;
            }
            Err(e) => {
                deliver_to_target(channel, target, cron_output, deliver_tx).await;
            }
        }
    }

#### New announce_to_session (REPLACE WITH)

    async fn announce_to_session(
        cron_name: &str,
        cron_message: &str,     // <-- NEW: original cron config message
        cron_output: &str,
        channel: &str,
        target: &str,
        agent_id: &str,
        router: &MessageRouter,
        deliver_tx: Option<&DeliverySender>,
    ) {
        // Groups: direct delivery (unchanged — no DM session for groups)
        if target.starts_with("group:") {
            deliver_to_target(channel, target, cron_output, deliver_tx).await;
            return;
        }

        let dm_session_key = SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Dm(format!("{channel}:{target}")),
        };

        // 1. Append context: user message with cron task
        let context_msg = Message::user().with_text(
            &format!("[Scheduled: {cron_name}]\n{cron_message}")
        );
        router.append_to_session(&dm_session_key, context_msg);

        // 2. Append cron output as assistant message
        let output_msg = Message::assistant().with_text(cron_output);
        router.append_to_session(&dm_session_key, output_msg);

        // 3. Deliver to channel
        deliver_to_target(channel, target, cron_output, deliver_tx).await;
    }

### Changes to router.rs

Add a thin delegation method to MessageRouter:

    pub(crate) fn append_to_session(&self, session_key: &SessionKey, message: Message) {
        self.gateway.append_message(session_key, message);
    }

This is a non-turn message append — no LLM call, no prompt building, no tool execution. Just writes to the session store and in-memory cache.

### Changes to fire_cron in scheduler.rs

Pass the original cron message (cfg.message) through to announce_to_session so it can be included in the context injection.

Current call site in fire_cron:

    for (channel, target) in &delivery_targets {
        announce_to_session(
            &cfg.name,
            &content,
            channel,
            target,
            &agent_id,
            router,
            deliver_tx,
        )
        .await;
    }

New call site:

    for (channel, target) in &delivery_targets {
        announce_to_session(
            &cfg.name,
            &cfg.message,      // original cron message for context
            &content,
            channel,
            target,
            &agent_id,
            router,
            deliver_tx,
        )
        .await;
    }

### What does NOT change

- Cron session execution (fire_cron stage 1) — unchanged
- HEARTBEAT_OK suppression — unchanged, still checked before announce
- should_skip_heartbeat (empty HEARTBEAT.md) — unchanged
- Group delivery — unchanged (direct delivery, no DM session)
- DeliverySender / spawn_signal_delivery_bridge — unchanged
- Config format — no new fields needed
- CronConfig struct — unchanged

### Cleanup

Delete these imports from scheduler.rs (no longer needed):
- SessionInjection (was used to build the announce injection)
- InjectionSource (was used for InjectionSource::Cron)

The injection.rs module and router's inject_collect_text/dispatch_injection methods stay — they're used by other features (inter-session messaging, future webhook triggers). Only the cron announce path stops using them.

### Session store writes

append_to_session calls gateway.append_message which both:
1. Writes to the in-memory session cache (Mutex<HashMap<SessionKey, Vec<Message>>>)
2. Appends to the JSONL file on disk via DiskSessionStore::append

This is the same path used when run_turn_with_trust appends user/assistant messages during normal turns. No new persistence code needed.

### Tracing

The existing cron_announce span should be updated. Currently it wraps an LLM turn. New behavior is simpler — just message append + delivery. Keep the span but update the events:

    async {
        info!(
            dm_session = %dm_session_key,
            "injecting cron output into DM session"
        );
        // ... append messages ...
        // ... deliver ...
    }
    .instrument(span)
    .await;

### Error handling

The only operation that can fail is delivery (DeliverySender::send). Message appending (gateway.append_message) logs warnings on disk write failure but doesn't return errors. This is simpler than the current flow which had to handle LLM call failures with a fallback to direct delivery. The fallback path is no longer needed — we always deliver the cron output directly.

## Test updates

### Tests to modify

These tests verify the announce flow behavior. Update them to check for direct message injection instead of an LLM turn:

1. **fire_cron_with_delivery_announces_to_dm_session** — verify that after fire_cron, the DM session contains exactly 2 messages: one User (context) and one Assistant (cron output). Currently checks that a DM session exists; update to check message content.

2. **fire_cron_announce_uses_dm_session_key** — currently checks cron session has 2 msgs and DM session has 2 msgs (from the LLM turn). Update: cron session still has 2 msgs (user + assistant from cron LLM call). DM session now has 2 msgs (injected user context + injected assistant output). Message counts stay the same but the DM messages are injected, not from an LLM call.

3. **fire_cron_announce_fallback_on_dm_error** — this test used FailOnSecondCallProvider to verify that when the DM session LLM call fails, raw cron output is delivered as fallback. Since there's no second LLM call anymore, this test becomes simpler: the cron output is always delivered directly. Can either remove the fallback test or convert it to verify that delivery works even when the provider would fail (since we never call the provider for announce).

4. **fire_cron_with_delivery_sends_response** — currently checks that the delivered message equals the FakeProvider response. This should still pass since we now deliver the cron output directly (which is the FakeProvider response).

5. **fire_cron_with_delivery_skips_empty_response** — currently checks that empty/whitespace responses are not delivered. This needs to still work. The empty check should happen before injection — if cron output is empty after stripping, skip both injection and delivery.

### Tests to add

1. **fire_cron_injects_context_into_dm_session** — verify the DM session contains a User message starting with "[Scheduled: {name}]" followed by the original cron message text.

2. **fire_cron_injects_output_as_assistant_message** — verify the DM session contains an Assistant message with the cron output text.

3. **fire_cron_does_not_inject_on_heartbeat_ok** — verify that when cron output is HEARTBEAT_OK, no messages are appended to the DM session.

4. **fire_cron_does_not_inject_on_empty_output** — verify that whitespace-only cron output results in no DM session injection and no delivery.

## Verify

After implementation:

1. cargo fmt
2. cargo build — must succeed
3. cargo test -p coop-gateway -- scheduler — all scheduler tests pass
4. cargo clippy --all-targets --all-features -- -D warnings — clean
5. Manual verification with COOP_TRACE_FILE=traces.jsonl: fire a cron job with delivery, confirm traces show one LLM call (cron session) and direct injection (no second provider_request span for the DM session)
