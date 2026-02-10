# Cron Delivery: Explicit Output Binding

Replace the "LLM decides to call signal_send" pattern for cron jobs with explicit, config-driven delivery. The scheduler should automatically send the agent's response text to a configured channel+target after each cron turn — the same way `signal_loop.rs` automatically sends responses back to whoever messaged.

Read `AGENTS.md` at the project root before starting. Follow the development loop and code quality rules.

## Problem

The current cron design relies on prompt engineering to get delivery right:

```toml
- name: morning-briefing
  cron: "0 8 * * *"
  user: alice
  message: |
    Morning briefing. Summarize overnight activity.
    Send the briefing to alice-uuid via signal_send.
```

This is fragile. The LLM might forget to call `signal_send`, call it with the wrong target, format the tool call incorrectly, or decide on its own not to send. Delivery should not be a suggestion — it should be a guarantee baked into the scheduler infrastructure, just like the signal loop guarantees every DM gets a response.

Cron jobs that do internal work (file maintenance, workspace cleanup) don't need delivery — they just run silently. Both modes must be supported.

## Design

### Config: `deliver` field

Add an optional `deliver` field to `CronConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CronDelivery {
    /// Channel to deliver through (e.g. "signal").
    pub channel: String,
    /// Target on that channel (e.g. a UUID for DM, "group:<hex>" for group).
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CronConfig {
    pub name: String,
    pub cron: String,
    pub message: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub deliver: Option<CronDelivery>,
}
```

TOML examples:

```toml
cron:
  # Delivers the agent's response to alice via Signal.
  - name: morning-briefing
    cron: "0 8 * * *"
    user: alice
    deliver:
      channel: signal
      target: "alice-uuid"
    message: |
      Morning briefing. Summarize overnight activity.

  # Delivers to a Signal group.
  - name: weekly-review
    cron: "0 18 * * 5"
    user: alice
    deliver:
      channel: signal
      target: "group:deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb"
    message: |
      Weekly review. Summarize the week.

  # No delivery — internal work, runs silently.
  - name: heartbeat
    cron: "*/30 * * * *"
    user: alice
    message: |
      Heartbeat check. Review HEARTBEAT.md for pending tasks.

  # No delivery, no user — system maintenance.
  - name: cleanup
    cron: "0 3 * * *"
    message: |
      System maintenance. Archive old log files.
```

The `message` field no longer needs to instruct the agent to call `signal_send`. It focuses on *what to do*, not *how to deliver*. Delivery is infrastructure, not prompt.

### Scheduler: auto-deliver after turn

Update `fire_cron()` in `scheduler.rs`. After `dispatch_collect_text` returns the response text, if `deliver` is configured, send the response through the appropriate channel. The scheduler needs access to a delivery mechanism — a way to send an `OutboundMessage` to a channel.

```rust
async fn fire_cron(cfg: &CronConfig, router: &MessageRouter, deliver_tx: Option<&DeliverySender>) {
    // ... existing span + inbound construction ...

    match router.dispatch_collect_text(&inbound).await {
        Ok((decision, response)) => {
            info!(
                session = %decision.session_key,
                trust = ?decision.trust,
                user = ?decision.user_name,
                "cron completed"
            );

            // Deliver response if configured and non-empty.
            if let Some(ref delivery) = cfg.deliver {
                if !response.trim().is_empty() {
                    deliver_response(delivery, &response, deliver_tx).await;
                } else {
                    debug!(cron.name = %cfg.name, "cron produced empty response, skipping delivery");
                }
            }
        }
        Err(e) => {
            error!(error = %e, "cron dispatch failed");
        }
    }
}
```

### Delivery mechanism

The scheduler doesn't own channels. It needs a way to send outbound messages. The simplest approach: pass the signal `action_tx` (or a more general delivery sender) into `run_scheduler`.

For now, Signal is the only delivery channel. Use the existing `SignalAction::SendText` path — the same one `signal_loop.rs` and `signal_send` tool use:

```rust
use coop_core::OutboundMessage;
use tokio::sync::mpsc;

/// Sender for delivering cron output to channels.
/// Currently only Signal is supported.
#[derive(Clone)]
pub(crate) struct DeliverySender {
    signal_tx: mpsc::Sender<coop_channels::SignalAction>,
}

impl DeliverySender {
    pub fn new(signal_tx: mpsc::Sender<coop_channels::SignalAction>) -> Self {
        Self { signal_tx }
    }

    pub async fn send(&self, channel: &str, target: &str, content: &str) -> Result<()> {
        match channel {
            "signal" => {
                let outbound = OutboundMessage {
                    channel: "signal".to_owned(),
                    target: target.to_owned(),
                    content: content.to_owned(),
                };
                self.signal_tx
                    .send(coop_channels::SignalAction::SendText(outbound))
                    .await
                    .map_err(|_send_err| anyhow::anyhow!("signal action channel closed"))
            }
            other => {
                anyhow::bail!("unsupported delivery channel: {other}");
            }
        }
    }
}
```

This is intentionally simple — one channel type for now. When other channels exist (email, webhook, etc.), `DeliverySender` gets new variants. The config already names the channel explicitly, so it's future-proof.

**Important:** `DeliverySender` depends on `coop_channels::SignalAction`, which is behind the `signal` feature flag. The delivery sender must be feature-gated the same way. When Signal isn't compiled in, `deliver_tx` is `None` and delivery config logs a warning at startup.

### Updated `run_scheduler` signature

```rust
pub(crate) async fn run_scheduler(
    cron: Vec<CronConfig>,
    router: Arc<MessageRouter>,
    users: &[UserConfig],
    deliver_tx: Option<DeliverySender>,
    shutdown: CancellationToken,
)
```

At startup, validate `deliver` configs:
- If `deliver.channel` is "signal" but no `deliver_tx` is available, log a warning and skip delivery for that entry (the turn still runs, just no delivery).
- If `deliver.channel` is an unknown channel, log an error and skip delivery for that entry.

### Wiring in `cmd_start()`

Pass the signal `action_tx` into the scheduler:

```rust
// In cmd_start(), when spawning the scheduler:
let deliver_tx = signal_action_tx.as_ref().map(|tx| scheduler::DeliverySender::new(tx.clone()));

tokio::spawn(async move {
    scheduler::run_scheduler(cron, sched_router, &users, deliver_tx, sched_token).await;
});
```

When signal isn't configured (or feature isn't compiled), `deliver_tx` is `None`. Cron entries with `deliver` will log a warning and skip delivery.

### Remove `signal_send` from cron-only registration

The `signal_send` tool was added specifically for cron delivery. With explicit delivery, it's no longer needed for that purpose. Remove the `include_send` parameter from `SignalToolExecutor::new()` and remove `SignalSendTool` entirely.

The `signal_send` tool was only gated for cron sessions — normal sessions never had it. With delivery handled by infrastructure, there's no remaining use case for `signal_send` as a tool. If a future need arises (agent-initiated messaging outside cron), it can be re-added with proper trust controls.

Revert `SignalToolExecutor::new()` to its original single-argument signature:

```rust
impl SignalToolExecutor {
    pub fn new(action_tx: mpsc::Sender<SignalAction>) -> Self {
        Self {
            tools: vec![
                Box::new(SignalReactTool::new(action_tx.clone())),
                Box::new(SignalReplyTool::new(action_tx)),
            ],
        }
    }
}
```

Update all call sites (in `main.rs` and `signal_loop/tests.rs`) to remove the `bool` argument.

### Delivery tracing

Add tracing events for delivery:

```rust
async fn deliver_response(delivery: &CronDelivery, response: &str, deliver_tx: Option<&DeliverySender>) {
    let Some(tx) = deliver_tx else {
        warn!(
            channel = %delivery.channel,
            target = %delivery.target,
            "cron delivery configured but no delivery sender available"
        );
        return;
    };

    let span = info_span!(
        "cron_deliver",
        channel = %delivery.channel,
        target = %delivery.target,
        content_len = response.len(),
    );

    async {
        match tx.send(&delivery.channel, &delivery.target, response).await {
            Ok(()) => {
                info!("cron delivery sent");
            }
            Err(e) => {
                error!(error = %e, "cron delivery failed");
            }
        }
    }
    .instrument(span)
    .await;
}
```

### System prompt hint

When a cron entry has `deliver` configured, the scheduler should prepend a short context line to the message content before injecting it, so the agent knows its response will be delivered:

```
[Your response will be delivered to {target} via {channel}.]

{original message}
```

This gives the agent context about its audience without relying on the prompt author to remember. For entries without `deliver`, no prefix is added — the agent's response is logged but not sent anywhere.

## Implementation Steps

1. **Add `CronDelivery` struct and `deliver` field** to `CronConfig` in `config.rs`. Add config parsing tests: with delivery, without delivery, delivery with group target.

2. **Add `DeliverySender`** to `scheduler.rs`. Feature-gate the Signal path behind `#[cfg(feature = "signal")]`. When signal isn't available, the struct is empty and `send()` always returns an error for "signal" channel.

3. **Update `fire_cron()`** to accept `Option<&DeliverySender>`. After getting the response from `dispatch_collect_text`, deliver it if `deliver` is configured and response is non-empty. Add the delivery context prefix to the message content.

4. **Update `run_scheduler()`** signature to accept `Option<DeliverySender>`. At startup, validate delivery configs and log warnings for unsupported channels or missing senders.

5. **Wire `DeliverySender` into `cmd_start()`** in `main.rs`. Construct from `signal_action_tx` when available.

6. **Remove `SignalSendTool`** from `signal_tools.rs`. Remove the `include_send` parameter from `SignalToolExecutor::new()`. Revert to original single-argument signature. Update all call sites.

7. **Update tests:**
   - `config.rs`: Parse cron with and without `deliver`. Verify `CronDelivery` fields.
   - `scheduler.rs`: Test `fire_cron` with delivery — verify `DeliverySender::send()` is called with correct channel/target/content. Test `fire_cron` without delivery — verify no send attempt. Test delivery with empty response — verify skip. Test delivery with no sender available — verify warning (no panic).
   - `signal_tools.rs`: Remove `signal_send` tests. Remove `include_send` tests. Verify executor has only `signal_react` and `signal_reply`.
   - `signal_loop/tests.rs`: Update `SignalToolExecutor::new()` calls to remove `bool` arg.
   - Integration tests: Scheduler fires with delivery config, verify the outbound message appears on the delivery channel with the correct target and the agent's response as content.

8. **Update cron config example** in the `cron-scheduler.md` prompt doc (or leave it — it's a historical record of the previous design). Update the example in `AGENTS.md` if it references cron.

9. **Verify build times.** No new dependencies — this is purely structural.

## Config Example (updated)

```toml
cron:
  # Delivers the agent's response to alice.
  - name: morning-briefing
    cron: "0 8 * * *"
    user: alice
    deliver:
      channel: signal
      target: "alice-uuid"
    message: |
      Morning briefing. Summarize:
      1. Any overnight messages that need attention
      2. Calendar items for today
      3. Any pending tasks from HEARTBEAT.md
      Keep it concise.

  # Delivers to a group.
  - name: weekly-review
    cron: "0 18 * * 5"
    user: alice
    deliver:
      channel: signal
      target: "group:deadbeef00112233445566778899aabbccddeeff00112233445566778899aabb"
    message: |
      Weekly review. Summarize the week's key events and decisions.

  # No delivery — silent internal work.
  - name: heartbeat
    cron: "*/30 * * * *"
    user: alice
    message: |
      Heartbeat check. Review HEARTBEAT.md for pending tasks.
      Execute any that are due. If nothing is due, do nothing.

  # No delivery, no user — system maintenance.
  - name: cleanup
    cron: "0 3 * * *"
    message: |
      System maintenance. Archive log files older than 7 days.
```

## Testing

### Unit tests

- `config.rs`: Parse config with `deliver` field. Verify channel and target. Parse config without `deliver` — field is `None`. Parse config with delivery to a group target.
- `scheduler.rs`:
  - `fire_cron` with delivery: mock `DeliverySender`, verify `send()` called with correct args and the agent's response text as content.
  - `fire_cron` without delivery: verify no send attempt.
  - `fire_cron` with delivery but empty response: verify delivery skipped.
  - `fire_cron` with delivery but no sender (`None`): verify no panic, warning logged.
  - `deliver_response` with unsupported channel: verify error.
  - Message content includes delivery context prefix when `deliver` is set.
  - Message content has no prefix when `deliver` is not set.
- `signal_tools.rs`: `SignalToolExecutor::new(tx)` returns only `signal_react` and `signal_reply`. No `signal_send` tool exists.

### Integration tests

- Scheduler fires cron with delivery config. After the turn completes, verify:
  - The cron session was created (existing test).
  - The delivery sender received an outbound message with the correct channel, target, and the FakeProvider's response text as content.
- Scheduler fires cron without delivery config. Verify the turn completes but no outbound message is sent.

## Not in Scope

- **Multi-channel delivery.** One `deliver` target per cron entry. If the agent needs to send to multiple targets, use multiple cron entries with the same schedule and message.
- **Delivery confirmation.** Fire-and-forget. If Signal is down, the delivery fails and is logged. No retry.
- **Conditional delivery.** The response is always delivered if non-empty. If the agent wants to suppress delivery, it returns empty text (which the scheduler already skips). This is an edge case — in practice, the LLM will always produce a response to a system message.
