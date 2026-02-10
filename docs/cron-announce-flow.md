# Cron Announce Flow — Design & Implementation Prompt

## Problem

Cron jobs currently deliver responses by dumping the raw cron agent text directly to the user's channel (Signal). This has three problems:

1. **No channel awareness** — the cron agent runs in a `cron:heartbeat` session with channel `"cron"`, so it doesn't get Signal formatting context. (Partially fixed: we now pass the delivery channel as a prompt channel override.)

2. **No conversational continuity** — cron responses arrive as disconnected messages. The main agent session (the one the user actually talks to on Signal) has no awareness that a cron fired or what it found. If the user replies to a cron delivery, that reply goes to the DM session, which has zero context about the cron results.

3. **No ability to interact** — a cron task like "ask alice how she's doing" can't work because the cron session is one-shot and isolated. It can't send messages via the user's channel and wait for replies.

## Solution: Announce Flow

Instead of delivering cron output directly to the channel, pipe it through the user's **main agent session** (the DM session on Signal). The main session has full channel context, conversational history, and the ability to interact naturally.

### Flow

```
Cron fires
  → Isolated agent turn runs in cron session (cron:heartbeat)
  → Agent produces text output
  → If HEARTBEAT_OK → suppress (no change)
  → Otherwise:
      → Inject cron results into the user's DM session via SessionInjection
      → Run a turn on the DM session with deliver=true
      → The DM session agent summarizes the cron findings naturally
      → Response is delivered via Signal (normal DM delivery path)
```

## SessionInjection — General-Purpose Internal Messaging

The announce flow needs to send a message into a session that didn't originate from an external channel. Rather than fabricating a fake `InboundMessage` (lying about the origin, populating irrelevant fields, depending on routing/trust resolution to happen to produce the right answer), we introduce `SessionInjection` — a first-class type for internal messages.

This is the general-purpose device for any code that needs to message a session: cron announce, inter-session messaging, webhook-triggered work, agent delegation, broadcast notifications.

### The type

Lives in `crates/coop-gateway/src/injection.rs` — gateway-internal, not in `coop-core`.

```rust
/// An internally-generated message injected directly into a session.
///
/// Unlike `InboundMessage` (from an external channel, needs routing),
/// injections target a known session. The caller specifies the target
/// session, trust, and context explicitly. The injection still flows
/// through the `MessageRouter` for policy enforcement.
pub(crate) struct SessionInjection {
    /// Target session (already known — no channel→session mapping needed).
    pub target: SessionKey,
    /// Content injected as a user message.
    pub content: String,
    /// Trust level for tool access during the resulting turn.
    pub trust: TrustLevel,
    /// User name for prompt context.
    pub user_name: Option<String>,
    /// Channel name for prompt formatting (e.g. "signal").
    /// Controls formatting instructions, not routing.
    pub prompt_channel: Option<String>,
    /// Where this injection originated (for tracing + policy decisions).
    pub source: InjectionSource,
}

/// Origin of a session injection — used for tracing and policy branching.
pub(crate) enum InjectionSource {
    /// A cron job announcing results. Carries the cron name.
    Cron(String),
    /// Another session (inter-session messaging). Carries the source session key.
    Session(SessionKey),
    /// System-level event (webhook, API, etc.).
    System,
}
```

### Why not fake an `InboundMessage`?

| Concern | Fake `InboundMessage` | `SessionInjection` |
|---|---|---|
| **Type honesty** | Lies about origin (pretends to be from Signal) | Explicitly says "this is internal" |
| **Routing** | Goes through router which re-derives the session key you already know | Target is explicit; router skips channel→session mapping |
| **Trust resolution** | Depends on the fake identity matching a full-trust user pattern | Caller specifies trust directly |
| **Irrelevant fields** | Must populate `is_group`, `chat_id`, `message_timestamp`, etc. | Only carries what matters |
| **Tracing** | Shows up as a "signal" message in traces (confusing) | `InjectionSource::Cron("heartbeat")` in spans — clear provenance |
| **Policy** | Policy layer can't distinguish real channel messages from fakes | Policy can branch on `InjectionSource` |
| **Extensibility** | Every new use case needs another fake identity scheme | Add a variant to `InjectionSource` |

### Router integration

All messages — external and internal — flow through the `MessageRouter`. The router is the single chokepoint for policy enforcement (trust gates, rate limiting, audit logging, future authorization rules). Injections don't bypass the router; they take a separate entry point that skips the routing/resolution steps (because the caller already knows the target) but still passes through policy.

```rust
impl MessageRouter {
    // Existing — external messages: route + resolve trust + enforce policy + run turn
    pub(crate) async fn dispatch(
        &self, msg: &InboundMessage, event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<RouteDecision>

    // New — internal messages: skip routing, enforce policy, run turn
    pub(crate) async fn dispatch_injection(
        &self,
        injection: &SessionInjection,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<RouteDecision> {
        let decision = RouteDecision {
            session_key: injection.target.clone(),
            trust: injection.trust,
            user_name: injection.user_name.clone(),
        };

        let span = info_span!(
            "route_injection",
            session = %decision.session_key,
            trust = ?decision.trust,
            source = ?injection.source,
        );

        // *** Policy enforcement happens here ***
        // Same hooks as dispatch: rate limiting, audit, future rules.
        // Can branch on injection.source for source-specific policies.

        self.gateway
            .run_turn_with_trust(
                &decision.session_key,
                &injection.content,
                decision.trust,
                decision.user_name.as_deref(),
                injection.prompt_channel.as_deref(),
                event_tx,
            )
            .instrument(span)
            .await?;

        Ok(decision)
    }

    // Convenience: injection + collect text response
    pub(crate) async fn inject_collect_text(
        &self,
        injection: &SessionInjection,
    ) -> Result<(RouteDecision, String)> {
        // Same channel/task pattern as dispatch_collect_text,
        // but calls dispatch_injection internally.
    }
}
```

#### What the router does for each path

| Step | `dispatch` (external) | `dispatch_injection` (internal) |
|---|---|---|
| **Route** (channel+sender → session) | Yes | No — target is explicit |
| **Trust resolution** (user match → trust level) | Yes | No — trust is explicit |
| **Policy enforcement** (trust gate, rate limit, audit) | Yes | Yes — same hooks, source-aware |
| **Tracing span** | `route_message` | `route_injection` (with `source` field) |
| **Run turn** | `run_turn_with_trust` | Same |

#### Policy can branch on source

When policies are added later, the router can inspect the source:

```rust
// Hypothetical future policy
fn check_rate_limit(&self, decision: &RouteDecision, source: Option<&InjectionSource>) -> bool {
    match source {
        Some(InjectionSource::Cron(_)) => true,  // cron is pre-authorized by config
        Some(InjectionSource::Session(from)) => self.check_session_quota(from),
        None => self.check_channel_rate(decision), // external channel limits
        _ => true,
    }
}
```

### Future uses this unlocks

1. **Session-to-session messaging** — a tool in session A posts results to session B via `InjectionSource::Session(session_a_key)`
2. **Webhook-triggered work** — HTTP endpoint injects into a named session via `InjectionSource::System`
3. **Agent delegation** — main agent farms out a subtask to an isolated session, collects the result
4. **Broadcast** — inject the same content into multiple sessions (e.g. "system maintenance in 5 minutes")

## Implementation: Cron Announce

### What Changes

**`crates/coop-gateway/src/injection.rs`** — New file

The `SessionInjection` and `InjectionSource` types defined above.

**`crates/coop-gateway/src/router.rs`** — New methods

Add `dispatch_injection` and `inject_collect_text` to `MessageRouter`.

**`crates/coop-gateway/src/scheduler.rs`** — `fire_cron` + `announce_to_session`

After the cron agent turn completes and we have a non-suppressed response, instead of calling `deliver_to_target` directly:

1. Build an announce `SessionInjection` targeting the user's DM session
2. Dispatch it through `router.inject_collect_text`
3. Deliver the DM session's response to the channel

The announce content injected into the DM session:

```
[Scheduled task "{cron_name}" completed. Summarize the findings naturally for the user. Keep it brief — this is a push notification. If nothing important, you can reply with just "noted" or similar. Do not mention that this was a scheduled task unless relevant.]

{cron_agent_output}
```

**`scheduler.rs` — new function `announce_to_session`**

```rust
async fn announce_to_session(
    cron_name: &str,
    cron_output: &str,
    channel: &str,
    target: &str,
    agent_id: &str,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
) {
    // Group targets can't use announce flow (trust ceiling = Familiar).
    // Fall back to direct delivery.
    if target.starts_with("group:") {
        deliver_to_target(channel, target, cron_output, deliver_tx).await;
        return;
    }

    let announce_content = format!(
        "[Scheduled task \"{}\" completed. Summarize the findings naturally \
         for the user. Keep it brief — this is a push notification. Do not \
         mention this was a scheduled task unless relevant.]\n\n{}",
        cron_name, cron_output
    );

    let injection = SessionInjection {
        target: SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Dm(format!("{channel}:{target}")),
        },
        content: announce_content,
        trust: TrustLevel::Full,
        user_name: None,
        prompt_channel: Some(channel.to_owned()),
        source: InjectionSource::Cron(cron_name.to_owned()),
    };

    match router.inject_collect_text(&injection).await {
        Ok((_decision, response)) => {
            if !response.trim().is_empty() {
                deliver_to_target(channel, target, &response, deliver_tx).await;
            }
        }
        Err(e) => {
            error!(error = %e, "cron announce dispatch failed");
            // Fall back to direct delivery of raw cron output
            deliver_to_target(channel, target, cron_output, deliver_tx).await;
        }
    }
}
```

### Handling the announce turn's response

The DM session's response to the announce is what gets delivered to Signal. This means:

- The DM session's prompt builder gets `channel = "signal"` via `prompt_channel`
- The agent gets Signal formatting instructions
- The agent has full conversation context from prior DM interactions
- The response is natural and in-voice

### Double-message prevention

Since we no longer call `deliver_to_target` with the raw cron output (we deliver the DM session's response instead), there's no double-message risk. The cron agent's output never goes directly to the channel — only the DM session's rephrased response does.

The `signal_send` tool is available in the DM session and works correctly there (unlike in cron sessions where `extract_signal_target_from_session` fails). If the DM agent calls `signal_send`, that's fine — it's the normal DM behavior.

### Heartbeat suppression

Heartbeat suppression (`HEARTBEAT_OK`) still happens at the cron level, *before* the announce flow. If the cron agent responds with `HEARTBEAT_OK`, we never enter the announce path. This avoids wasting a DM session turn on "nothing to report."

### Session accumulation concern

Each cron announce adds messages to the DM session. Over time, this grows the session history. This is acceptable because:

1. The DM session already has compaction logic that kicks in when context grows too large
2. The announce messages become part of the conversation context, which is actually desirable — the agent remembers what cron tasks it reported

### Error handling

If the DM session turn fails (provider error, etc.):
- Fall back to direct delivery of the raw cron output (current behavior)
- Log the error for tracing

### Config implications

No config changes needed. The existing cron config works as-is:
- `user: "alice"` → resolves alice's match patterns → finds `signal:alice-uuid` → constructs `SessionKey { kind: Dm("signal:alice-uuid") }` directly
- `deliver.channel: "signal"` / `deliver.target: "alice-uuid"` → same thing

### Group delivery targets

Group targets (`signal:group:deadbeef`) cannot use the announce flow because:
1. Groups don't have a single "DM session" to announce through
2. The trust ceiling for groups is Familiar, not Full

For group delivery targets, keep the current direct-delivery behavior (deliver the raw cron output). Only DM targets use the announce flow.

In `announce_to_session`, check if the target looks like a group (`target.starts_with("group:")`) and fall back to direct delivery for groups.

### What NOT to change

- `InboundMessage` stays as-is — it's the right type for external channel messages
- `route_message()` stays — it's the right function for external routing
- `dispatch_collect_text()` stays — it's the right method for external messages that need routing
- The cron agent still runs in its own isolated session (`cron:heartbeat`). It still uses `dispatch_collect_text_with_channel` with the signal prompt channel override for formatting.
- The `DeliverySender` and `spawn_signal_delivery_bridge` stay. Direct delivery is used for the final DM response.
- Heartbeat file skip logic (`should_skip_heartbeat`) stays at the cron level.
- `strip_heartbeat_token` stays at the cron level.

### Test plan

1. **Unit test**: `fire_cron_with_delivery_announces_to_dm_session` — verify that when a cron has delivery targets, the response is dispatched through the DM session before delivery
2. **Unit test**: `fire_cron_announce_fallback_on_dm_error` — verify that if the DM dispatch fails, the raw cron output is delivered directly
3. **Unit test**: `fire_cron_heartbeat_ok_skips_announce` — verify HEARTBEAT_OK still suppresses before reaching the announce path
4. **Unit test**: `fire_cron_announce_uses_dm_session_key` — verify the announce message targets `SessionKind::Dm` not `SessionKind::Cron`
5. **Unit test**: `inject_collect_text_runs_turn_on_target_session` — verify the injection runs a turn on the specified session key
6. **Unit test**: `inject_collect_text_uses_explicit_trust` — verify the injection uses the trust level from `SessionInjection`, not from routing
7. **Existing tests**: All existing scheduler tests should still pass (they use `FakeProvider` which returns canned text)

### File changes summary

| File | Change |
|------|--------|
| `crates/coop-gateway/src/injection.rs` | New file: `SessionInjection`, `InjectionSource` types |
| `crates/coop-gateway/src/router.rs` | Add `dispatch_injection` and `inject_collect_text` methods to `MessageRouter` |
| `crates/coop-gateway/src/scheduler.rs` | Replace `deliver_to_target` calls in `fire_cron` with `announce_to_session`. Add `announce_to_session` function. |

### Incremental delivery

This can ship as a single PR. The injection infrastructure (`injection.rs` + router methods) is minimal new code. The scheduler change is isolated to `fire_cron`'s delivery path. All existing infrastructure (sessions, turns, delivery) is reused.
