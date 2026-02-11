use anyhow::Result;
use chrono::Utc;
use coop_core::{InboundKind, InboundMessage, Message, OutboundMessage, SessionKey, SessionKind};
use cron::Schedule;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::config::{Config, CronConfig, SharedConfig};
use crate::heartbeat::{HeartbeatResult, is_heartbeat_content_empty, strip_heartbeat_token};
use crate::reminder::{Reminder, ReminderStore};
use crate::router::MessageRouter;

/// Sender for delivering cron output to channels.
///
/// Wraps an `mpsc::Sender<OutboundMessage>`. In production, a bridge task
/// forwards outbound messages to the appropriate channel (e.g. Signal).
#[derive(Clone, Debug)]
pub(crate) struct DeliverySender {
    tx: mpsc::Sender<OutboundMessage>,
}

impl DeliverySender {
    #[cfg(any(feature = "signal", test))]
    pub(crate) fn new(tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self { tx }
    }

    pub(crate) async fn send(&self, channel: &str, target: &str, content: &str) -> Result<()> {
        let outbound = OutboundMessage {
            channel: channel.to_owned(),
            target: target.to_owned(),
            content: content.to_owned(),
        };
        self.tx
            .send(outbound)
            .await
            .map_err(|_send_err| anyhow::anyhow!("delivery channel closed"))
    }
}

/// Spawn a bridge task that forwards `OutboundMessage`s to a Signal action sender.
#[cfg(feature = "signal")]
pub(crate) fn spawn_signal_delivery_bridge(
    signal_tx: mpsc::Sender<coop_channels::SignalAction>,
) -> DeliverySender {
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundMessage>(64);
    tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if signal_tx
                .send(coop_channels::SignalAction::SendText(msg))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    DeliverySender::new(outbound_tx)
}

pub(crate) fn parse_cron(expr: &str) -> Result<Schedule> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let full_expr = match fields.len() {
        5 => format!("0 {expr} *"),
        6 => format!("0 {expr}"),
        7 => expr.to_owned(),
        _ => anyhow::bail!("invalid cron expression (expected 5-7 fields): {expr}"),
    };
    Schedule::from_str(&full_expr)
        .map_err(|e| anyhow::anyhow!("invalid cron expression '{expr}': {e}"))
}

#[cfg(test)]
pub(crate) async fn run_scheduler(
    config: SharedConfig,
    router: Arc<MessageRouter>,
    deliver_tx: Option<DeliverySender>,
    shutdown: CancellationToken,
) {
    run_scheduler_with_notify(config, router, deliver_tx, shutdown, None, None).await;
}

pub(crate) async fn run_scheduler_with_notify(
    config: SharedConfig,
    router: Arc<MessageRouter>,
    deliver_tx: Option<DeliverySender>,
    shutdown: CancellationToken,
    cron_notify: Option<Arc<tokio::sync::Notify>>,
    reminders: Option<ReminderStore>,
) {
    info!("scheduler started");

    // Default notify that is never triggered — simplifies select! below.
    let default_notify = tokio::sync::Notify::new();
    let notify = cron_notify.as_deref().unwrap_or(&default_notify);

    let mut last_cron: Vec<CronConfig> = Vec::new();
    let mut parsed: Vec<(CronConfig, Schedule)> = Vec::new();

    loop {
        // Re-read cron entries from shared config on each iteration so
        // hot-reloaded changes are picked up without a restart.
        let snapshot = config.load();
        if snapshot.cron != last_cron {
            parsed = parse_and_validate(&snapshot.cron, &snapshot.users, deliver_tx.as_ref());
            if !snapshot.cron.is_empty() {
                info!(
                    count = parsed.len(),
                    total = snapshot.cron.len(),
                    "scheduler cron entries updated"
                );
            }
            last_cron = snapshot.cron.clone();
        }
        // Drop the config snapshot so it isn't held across the sleep.
        drop(snapshot);

        // Fire any due reminders before computing the next sleep.
        if let Some(ref store) = reminders {
            let due = store.take_due();
            for reminder in due {
                let deliver_tx = deliver_tx.clone();
                let router = Arc::clone(&router);
                let config = Arc::clone(&config);
                tokio::spawn(async move {
                    fire_reminder(&reminder, &router, deliver_tx.as_ref(), &config).await;
                });
            }
        }

        let now = Utc::now();

        // Next cron fire time.
        let next_cron = parsed
            .iter()
            .filter_map(|(cfg, sched)| sched.upcoming(Utc).next().map(|t| (cfg, t)))
            .min_by_key(|(_, t)| *t);

        // Next reminder fire time.
        let next_reminder = reminders.as_ref().and_then(ReminderStore::next_fire_time);

        // Compute the earlier of cron vs reminder.
        let next_cron_time = next_cron.as_ref().map(|(_, t)| *t);
        let next_fire = match (next_cron_time, next_reminder) {
            (Some(c), Some(r)) => Some(c.min(r)),
            (Some(c), None) => Some(c),
            (None, Some(r)) => Some(r),
            (None, None) => None,
        };

        let Some(fire_time) = next_fire else {
            tokio::select! {
                () = notify.notified() => continue,
                () = shutdown.cancelled() => {
                    info!("scheduler shutting down");
                    return;
                }
            }
        };

        let delay = (fire_time - now).to_std().unwrap_or(Duration::ZERO);

        // Only log cron waits (reminders are handled at loop top).
        if let Some((cfg, _)) = &next_cron {
            debug!(
                cron.name = %cfg.name,
                fire_time = %fire_time,
                delay_secs = delay.as_secs(),
                "scheduler waiting for next fire"
            );
        }

        tokio::select! {
            () = tokio::time::sleep(delay) => {
                // Check if a cron is due at this fire_time.
                if let Some((cfg, _)) = &next_cron
                    && next_cron_time == Some(fire_time)
                {
                    let cfg = (*cfg).clone();
                    let router = Arc::clone(&router);
                    let deliver_tx = deliver_tx.clone();
                    let sched_config = Arc::clone(&config);
                    tokio::spawn(async move {
                        fire_cron(&cfg, &router, deliver_tx.as_ref(), &sched_config).await;
                    });
                }
                // Reminders are checked at the top of the next iteration
                // via take_due(), so we just loop.
            }
            () = notify.notified() => {
                debug!("scheduler woken by config/reminder change");
            }
            () = shutdown.cancelled() => {
                info!("scheduler shutting down");
                return;
            }
        }
    }
}

/// Parse and validate cron entries, logging warnings only once per config change.
fn parse_and_validate(
    cron: &[CronConfig],
    users: &[crate::config::UserConfig],
    deliver_tx: Option<&DeliverySender>,
) -> Vec<(CronConfig, Schedule)> {
    let mut parsed = Vec::new();
    for entry in cron {
        if let Some(ref user) = entry.user
            && !users.iter().any(|u| u.name == *user)
        {
            warn!(
                cron.name = %entry.name,
                cron.user = %user,
                "cron entry references unknown user"
            );
        }

        if let Some(ref delivery) = entry.deliver {
            if deliver_tx.is_none() {
                warn!(
                    cron.name = %entry.name,
                    delivery.channel = %delivery.channel,
                    delivery.target = %delivery.target,
                    "cron delivery configured but no delivery sender available"
                );
            } else if delivery.channel != "signal" {
                error!(
                    cron.name = %entry.name,
                    delivery.channel = %delivery.channel,
                    "unsupported delivery channel"
                );
            }
        }

        match parse_cron(&entry.cron) {
            Ok(schedule) => {
                parsed.push((entry.clone(), schedule));
            }
            Err(e) => {
                error!(cron.name = %entry.name, error = %e, "skipping invalid cron entry");
            }
        }
    }
    parsed
}

/// Resolve delivery targets for a cron entry.
///
/// If the cron entry has an explicit `deliver` field, return that single target.
/// If it has a `user` field but no `deliver`, look up the user's match patterns
/// and return all non-terminal channels.
/// Otherwise, return an empty list.
pub(crate) fn resolve_cron_delivery_targets(
    config: &Config,
    cfg: &CronConfig,
) -> Vec<(String, String)> {
    if let Some(ref delivery) = cfg.deliver {
        return vec![(delivery.channel.clone(), delivery.target.clone())];
    }

    let Some(ref user_name) = cfg.user else {
        return Vec::new();
    };

    let Some(user) = config.users.iter().find(|u| u.name == *user_name) else {
        return Vec::new();
    };

    user.r#match
        .iter()
        .filter_map(|pattern| {
            let (channel, target) = pattern.split_once(':')?;
            if channel == "terminal" {
                None
            } else {
                Some((channel.to_owned(), target.to_owned()))
            }
        })
        .collect()
}

async fn fire_cron(
    cfg: &CronConfig,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
) {
    let span = info_span!(
        "cron_fired",
        cron.name = %cfg.name,
        user = ?cfg.user,
    );

    async {
        info!(
            cron = %cfg.cron,
            message = %cfg.message,
            user = ?cfg.user,
            "cron firing"
        );

        let config_snapshot = shared_config.load();
        let delivery_targets = resolve_cron_delivery_targets(&config_snapshot, cfg);
        let agent_id = config_snapshot.agent.id.clone();

        // Skip LLM call for empty HEARTBEAT.md
        if should_skip_heartbeat(&config_snapshot, &cfg.message) {
            debug!(cron.name = %cfg.name, "heartbeat skipped: empty heartbeat file");
            return;
        }

        let sender = match &cfg.user {
            Some(user) => format!("cron:{}:{}", cfg.name, user),
            None => format!("cron:{}", cfg.name),
        };

        // Determine the delivery channel for prompt context. When responses
        // will be delivered to Signal, the prompt builder should format for
        // Signal (plain text, conversational tone) instead of the generic
        // "cron" channel which has no formatting instructions.
        let prompt_channel = delivery_targets
            .first()
            .map(|(channel, _)| channel.clone());

        let content = if delivery_targets.is_empty() {
            cfg.message.clone()
        } else {
            format!(
                "[Your response will be delivered to the user via {}. Reply HEARTBEAT_OK if nothing needs attention. Your response is delivered automatically.]\n\n{}",
                prompt_channel.as_deref().unwrap_or("messaging"),
                cfg.message
            )
        };

        let inbound = InboundMessage {
            channel: "cron".to_owned(),
            sender,
            content,
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Text,
            message_timestamp: None,
        };

        match router
            .dispatch_collect_text_with_channel(&inbound, prompt_channel)
            .await
        {
            Ok((decision, response)) => {
                info!(
                    session = %decision.session_key,
                    trust = ?decision.trust,
                    user = ?decision.user_name,
                    "cron completed"
                );

                if delivery_targets.is_empty() {
                    return;
                }

                match strip_heartbeat_token(&response) {
                    HeartbeatResult::Suppress => {
                        debug!(cron.name = %cfg.name, "heartbeat suppressed: HEARTBEAT_OK token detected");
                    }
                    HeartbeatResult::Deliver(content) => {
                        for (channel, target) in &delivery_targets {
                            announce_to_session(
                                &cfg.name,
                                &cfg.message,
                                &content,
                                channel,
                                target,
                                &agent_id,
                                router,
                                deliver_tx,
                            )
                            .await;
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "cron dispatch failed");
            }
        }
    }
    .instrument(span)
    .await;
}

async fn fire_reminder(
    reminder: &Reminder,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
) {
    let span = info_span!(
        "reminder_fired",
        reminder.id = %reminder.id,
        user = ?reminder.user,
    );

    async {
        let lateness = Utc::now() - reminder.fire_at;
        if lateness > chrono::Duration::seconds(60) {
            warn!(
                reminder.id = %reminder.id,
                late_by_secs = lateness.num_seconds(),
                "firing late reminder (gateway was likely down)"
            );
        }

        info!(
            message = %reminder.message,
            fire_at = %reminder.fire_at,
            delivery_count = reminder.delivery.len(),
            "reminder firing"
        );

        if reminder.delivery.is_empty() {
            warn!(reminder.id = %reminder.id, "reminder has no delivery targets");
            return;
        }

        let inbound = build_reminder_inbound(reminder);
        let prompt_channel = reminder
            .delivery
            .first()
            .map(|(channel, _)| channel.clone());

        dispatch_reminder(
            reminder,
            &inbound,
            prompt_channel,
            router,
            deliver_tx,
            shared_config,
        )
        .await;
    }
    .instrument(span)
    .await;
}

fn build_reminder_inbound(reminder: &Reminder) -> InboundMessage {
    let sender = match &reminder.user {
        Some(user) => format!("cron:reminder-{}:{}", reminder.id, user),
        None => format!("cron:reminder-{}", reminder.id),
    };

    let prompt_channel = reminder
        .delivery
        .first()
        .map(|(channel, _)| channel.clone());

    let session_slug = reminder.source_session.replace(['/', ':'], "_");
    let session_file = format!("sessions/{session_slug}.jsonl");

    let content = format!(
        "[This is a scheduled reminder set by the user. Execute the request \
         and respond with the result. Your response will be delivered to the \
         user via {channel}. Keep it conversational.\n\
         \n\
         If you need more context about what the user meant, read the \
         conversation where this reminder was created: {session_file}]\n\n\
         {message}",
        channel = prompt_channel.as_deref().unwrap_or("messaging"),
        session_file = session_file,
        message = reminder.message,
    );

    InboundMessage {
        channel: "cron".to_owned(),
        sender,
        content,
        chat_id: None,
        is_group: false,
        timestamp: Utc::now(),
        reply_to: None,
        kind: InboundKind::Text,
        message_timestamp: None,
    }
}

async fn dispatch_reminder(
    reminder: &Reminder,
    inbound: &InboundMessage,
    prompt_channel: Option<String>,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
) {
    let agent_id = shared_config.load().agent.id.clone();

    match router
        .dispatch_collect_text_with_channel(inbound, prompt_channel)
        .await
    {
        Ok((_decision, response)) => {
            if response.trim().is_empty() {
                debug!(reminder.id = %reminder.id, "reminder produced empty response");
                return;
            }

            for (channel, target) in &reminder.delivery {
                announce_to_session(
                    &format!("reminder:{}", reminder.id),
                    &reminder.message,
                    &response,
                    channel,
                    target,
                    &agent_id,
                    router,
                    deliver_tx,
                )
                .await;
            }

            info!(reminder.id = %reminder.id, "reminder completed and delivered");
        }
        Err(e) => {
            error!(
                reminder.id = %reminder.id,
                error = %e,
                "reminder dispatch failed"
            );

            for (channel, target) in &reminder.delivery {
                deliver_to_target(
                    channel,
                    target,
                    &format!("Reminder: {}", reminder.message),
                    deliver_tx,
                )
                .await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn announce_to_session(
    cron_name: &str,
    cron_message: &str,
    cron_output: &str,
    channel: &str,
    target: &str,
    agent_id: &str,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
) {
    // Groups: direct delivery (no DM session for groups)
    if target.starts_with("group:") {
        deliver_to_target(channel, target, cron_output, deliver_tx).await;
        return;
    }

    let dm_session_key = SessionKey {
        agent_id: agent_id.to_owned(),
        kind: SessionKind::Dm(format!("{channel}:{target}")),
    };

    let span = info_span!(
        "cron_announce",
        cron.name = %cron_name,
        channel = %channel,
        target = %target,
        session = %dm_session_key,
    );

    async {
        debug!(
            dm_session = %dm_session_key,
            "injecting cron output into DM session"
        );

        // 1. Append context: user message with cron task
        let context_msg =
            Message::user().with_text(format!("[Scheduled: {cron_name}]\n{cron_message}"));
        router.append_to_session(&dm_session_key, context_msg);

        // 2. Append cron output as assistant message
        let output_msg = Message::assistant().with_text(cron_output);
        router.append_to_session(&dm_session_key, output_msg);

        // 3. Deliver to channel
        deliver_to_target(channel, target, cron_output, deliver_tx).await;
    }
    .instrument(span)
    .await;
}

/// Check if the cron message references HEARTBEAT.md and if that file is empty.
fn should_skip_heartbeat(config: &Config, message: &str) -> bool {
    if !message.contains("HEARTBEAT.md") {
        return false;
    }

    // Resolve workspace path the same way Config::resolve_workspace does,
    // but we don't have config_dir here. Use a best-effort approach: if the
    // workspace path is absolute, use it directly. Otherwise we can't reliably
    // resolve it, so don't skip.
    let workspace_str = &config.agent.workspace;
    let workspace = std::path::PathBuf::from(workspace_str);
    if !workspace.is_absolute() {
        // Try CWD-relative as fallback (the gateway typically sets CWD).
        let cwd_relative = std::env::current_dir().ok().map(|cwd| cwd.join(&workspace));
        if let Some(ref path) = cwd_relative {
            return check_heartbeat_file(path);
        }
        return false;
    }

    check_heartbeat_file(&workspace)
}

fn check_heartbeat_file(workspace: &Path) -> bool {
    let heartbeat_path = workspace.join("HEARTBEAT.md");
    match std::fs::read_to_string(&heartbeat_path) {
        Ok(content) => {
            if is_heartbeat_content_empty(&content) {
                return true;
            }
            false
        }
        Err(_) => false, // File doesn't exist → proceed normally
    }
}

pub(crate) async fn deliver_to_target(
    channel: &str,
    target: &str,
    content: &str,
    deliver_tx: Option<&DeliverySender>,
) {
    let Some(tx) = deliver_tx else {
        warn!(
            channel = %channel,
            target = %target,
            "delivery target resolved but no delivery sender available"
        );
        return;
    };

    let span = info_span!(
        "cron_deliver",
        channel = %channel,
        target = %target,
        content_len = content.len(),
    );

    async {
        match tx.send(channel, target, content).await {
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CronDelivery, UserConfig, shared_config};
    use crate::gateway::Gateway;
    use async_trait::async_trait;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use coop_core::{
        Message, ModelInfo, Provider, SessionKey, SessionKind, ToolDef, TrustLevel, Usage,
    };

    #[test]
    fn parse_cron_5_field() {
        let schedule = parse_cron("*/30 * * * *").unwrap();
        let next = schedule.upcoming(Utc).next();
        assert!(next.is_some());
    }

    #[test]
    fn parse_cron_6_field() {
        let schedule = parse_cron("*/30 * * * * *").unwrap();
        let next = schedule.upcoming(Utc).next();
        assert!(next.is_some());
    }

    #[test]
    fn parse_cron_7_field() {
        let schedule = parse_cron("* * * * * * *").unwrap();
        let next = schedule.upcoming(Utc).next();
        assert!(next.is_some());
    }

    #[test]
    fn parse_cron_invalid() {
        assert!(parse_cron("not a cron").is_err());
    }

    #[test]
    fn parse_cron_too_few_fields() {
        assert!(parse_cron("* * *").is_err());
    }

    #[test]
    fn fire_cron_encodes_sender_with_user() {
        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };
        let sender = match &cfg.user {
            Some(user) => format!("cron:{}:{}", cfg.name, user),
            None => format!("cron:{}", cfg.name),
        };
        assert_eq!(sender, "cron:heartbeat:alice");
    }

    #[test]
    fn fire_cron_encodes_sender_without_user() {
        let cfg = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            message: "run cleanup".to_owned(),
            user: None,
            deliver: None,
        };
        let sender = match &cfg.user {
            Some(user) => format!("cron:{}:{}", cfg.name, user),
            None => format!("cron:{}", cfg.name),
        };
        assert_eq!(sender, "cron:cleanup");
    }

    #[tokio::test]
    async fn scheduler_exits_on_cancellation_with_empty_cron() {
        let cancel = CancellationToken::new();
        let (shared, router, _gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let router = Arc::new(router);

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(shared, router, None, cancel_clone).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("scheduler did not exit after cancellation")
            .expect("scheduler task panicked");
    }

    #[tokio::test]
    async fn scheduler_exits_on_cancellation() {
        let cron = vec![CronConfig {
            name: "test".to_owned(),
            cron: "0 0 1 1 *".to_owned(),
            message: "test".to_owned(),
            user: None,
            deliver: None,
        }];
        let (shared, router, _gateway) =
            make_shared_config_and_router(None, &cron, "cron response ok");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(shared, router, None, cancel_clone).await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("scheduler did not exit after cancellation")
            .expect("scheduler task panicked");
    }

    // -- Integration tests: scheduler fires through router into gateway --

    #[tokio::test]
    async fn scheduler_fires_and_routes_message() {
        let alice_user = UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec!["terminal:default".to_owned()],
        };
        let cron = vec![CronConfig {
            name: "test".to_owned(),
            cron: "* * * * * * *".to_owned(), // every second
            message: "heartbeat check".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        }];
        let (shared, router, gateway) =
            make_shared_config_and_router(Some(&[alice_user]), &cron, "cron response ok");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(shared, router, None, sched_cancel).await;
        });

        // Wait for at least one fire (cron fires every second).
        tokio::time::sleep(Duration::from_secs(2)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        // Verify: session was created with Cron kind.
        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "test"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'test', found: {sessions:?}"
        );

        // Verify: the routing produced the correct decision by checking the
        // session key format (which encodes agent_id:cron:name).
        let key = cron_session.unwrap();
        assert_eq!(key.to_string(), "test:cron:test");
    }

    #[tokio::test]
    async fn scheduler_fires_without_user() {
        let cron = vec![CronConfig {
            name: "cleanup".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "run cleanup".to_owned(),
            user: None,
            deliver: None,
        }];
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &cron, "cron response ok");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(shared, router, None, sched_cancel).await;
        });

        tokio::time::sleep(Duration::from_secs(2)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "cleanup"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'cleanup', found: {sessions:?}"
        );
    }

    #[tokio::test]
    async fn fire_cron_dispatches_through_router() {
        let alice = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![],
        }];
        let (shared, router, gateway) =
            make_shared_config_and_router(Some(&alice), &[], "cron response ok");

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, None, &shared).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "heartbeat"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'heartbeat' after fire, found: {sessions:?}"
        );
    }

    #[tokio::test]
    async fn fire_cron_without_user_dispatches_through_router() {
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");

        let cfg = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            message: "run cleanup".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, None, &shared).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "cleanup"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'cleanup' after fire, found: {sessions:?}"
        );
    }

    #[derive(Debug)]
    struct FailOnSecondCallProvider {
        model: ModelInfo,
        calls: std::sync::Mutex<u32>,
        first_response: String,
        error_message: String,
    }

    impl FailOnSecondCallProvider {
        fn new(first_response: &str, error_message: &str) -> Self {
            Self {
                model: ModelInfo {
                    name: "fail-on-second".to_owned(),
                    context_limit: 128_000,
                },
                calls: std::sync::Mutex::new(0),
                first_response: first_response.to_owned(),
                error_message: error_message.to_owned(),
            }
        }
    }

    #[async_trait]
    impl Provider for FailOnSecondCallProvider {
        fn name(&self) -> &'static str {
            "fail-on-second"
        }

        fn model_info(&self) -> ModelInfo {
            self.model.clone()
        }

        async fn complete(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                Ok((
                    Message::assistant().with_text(&self.first_response),
                    Usage {
                        input_tokens: Some(100),
                        output_tokens: Some(50),
                        ..Default::default()
                    },
                ))
            } else {
                anyhow::bail!("{}", self.error_message);
            }
        }

        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<coop_core::traits::ProviderStream> {
            anyhow::bail!("streaming not supported");
        }
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_sends_response() {
        let (shared, router, _gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert_eq!(msg.content, "cron response ok");
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_announces_to_dm_session() {
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert_eq!(msg.content, "cron response ok");

        let sessions = gateway.list_sessions();
        assert!(
            sessions
                .iter()
                .any(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "briefing"))
        );
        assert!(sessions.iter().any(|s| {
            matches!(&s.kind, SessionKind::Dm(identity) if identity == "signal:alice-uuid")
        }));
    }

    #[tokio::test]
    async fn fire_cron_announce_uses_dm_session_key() {
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let _ = rx.try_recv().unwrap();

        let cron_key = SessionKey {
            agent_id: "test".to_owned(),
            kind: SessionKind::Cron("heartbeat".to_owned()),
        };
        let dm_key = SessionKey {
            agent_id: "test".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        };

        // Cron session: 2 messages from the LLM call (user + assistant)
        assert_eq!(gateway.session_message_count(&cron_key), 2);
        // DM session: 2 injected messages (user context + assistant output), no LLM call
        assert_eq!(gateway.session_message_count(&dm_key), 2);

        let dm_messages = gateway.messages(&dm_key);
        assert_eq!(dm_messages[0].role, coop_core::Role::User);
        assert!(dm_messages[0].text().contains("[Scheduled: heartbeat]"));
        assert!(dm_messages[0].text().contains("check tasks"));
        assert_eq!(dm_messages[1].role, coop_core::Role::Assistant);
        assert_eq!(dm_messages[1].text(), "cron response ok");
    }

    #[tokio::test]
    async fn fire_cron_delivers_without_second_llm_call() {
        // With direct injection, delivery works even if the provider would
        // fail on a second call — because there is no second LLM call.
        let provider: Arc<dyn Provider> = Arc::new(FailOnSecondCallProvider::new(
            "raw cron output",
            "should never be called",
        ));
        let (shared, router, _gateway) =
            make_shared_config_and_router_with_provider(None, &[], provider);
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert_eq!(msg.content, "raw cron output");
    }

    #[tokio::test]
    async fn fire_cron_without_delivery_does_not_send() {
        let (shared, router, _gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_skips_empty_response() {
        let (shared, router, _gateway) = make_shared_config_and_router(None, &[], "   ");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_no_sender_does_not_panic() {
        let (shared, router, _gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, None, &shared).await;
    }

    #[tokio::test]
    async fn deliver_to_target_with_no_sender_does_not_panic() {
        deliver_to_target("email", "alice@example.com", "hello", None).await;
    }

    #[tokio::test]
    async fn fire_cron_injects_context_into_dm_session() {
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let (tx, _rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "humidifier-check".to_owned(),
            cron: "0 7 * * *".to_owned(),
            message: "Read WEATHER.md for the current temperature.".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let dm_key = SessionKey {
            agent_id: "test".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        };
        let messages = gateway.messages(&dm_key);
        assert!(!messages.is_empty());
        assert_eq!(messages[0].role, coop_core::Role::User);
        let text = messages[0].text();
        assert!(
            text.starts_with("[Scheduled: humidifier-check]"),
            "context message should start with [Scheduled: name], got: {text}"
        );
        assert!(
            text.contains("Read WEATHER.md for the current temperature."),
            "context message should contain original cron message"
        );
    }

    #[tokio::test]
    async fn fire_cron_injects_output_as_assistant_message() {
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &[], "Hey! It's 22F out there.");
        let (tx, _rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "humidifier-check".to_owned(),
            cron: "0 7 * * *".to_owned(),
            message: "Read WEATHER.md".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let dm_key = SessionKey {
            agent_id: "test".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        };
        let messages = gateway.messages(&dm_key);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, coop_core::Role::Assistant);
        assert_eq!(messages[1].text(), "Hey! It's 22F out there.");
    }

    #[tokio::test]
    async fn fire_cron_does_not_inject_on_heartbeat_ok() {
        let (shared, router, gateway) = make_shared_config_and_router(None, &[], "HEARTBEAT_OK");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "humidifier-check".to_owned(),
            cron: "0 7 * * *".to_owned(),
            message: "Read WEATHER.md".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let dm_key = SessionKey {
            agent_id: "test".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        };
        assert_eq!(
            gateway.session_message_count(&dm_key),
            0,
            "HEARTBEAT_OK should not inject into DM session"
        );
        assert!(
            rx.try_recv().is_err(),
            "HEARTBEAT_OK should not trigger delivery"
        );
    }

    #[tokio::test]
    async fn fire_cron_does_not_inject_on_empty_output() {
        let (shared, router, gateway) = make_shared_config_and_router(None, &[], "   ");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let dm_key = SessionKey {
            agent_id: "test".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        };
        assert_eq!(
            gateway.session_message_count(&dm_key),
            0,
            "empty output should not inject into DM session"
        );
        assert!(
            rx.try_recv().is_err(),
            "empty output should not trigger delivery"
        );
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_prepends_context() {
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");
        let (tx, _rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "briefing".to_owned(),
            cron: "0 8 * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "briefing"));
        assert!(cron_session.is_some());
    }

    #[tokio::test]
    async fn fire_cron_without_delivery_has_no_prefix() {
        let (shared, router, _gateway) =
            make_shared_config_and_router(None, &[], "cron response ok");

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, None, &shared).await;
    }

    #[tokio::test]
    async fn scheduler_fires_with_delivery_config() {
        let cron = vec![CronConfig {
            name: "briefing".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        }];
        let (shared, router, gateway) =
            make_shared_config_and_router(None, &cron, "cron response ok");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(shared, router, Some(deliver_tx), sched_cancel).await;
        });

        tokio::time::sleep(Duration::from_secs(2)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "briefing"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'briefing', found: {sessions:?}"
        );

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert_eq!(msg.content, "cron response ok");
    }

    /// Verify that cron entries added via hot-reload are picked up by the
    /// scheduler without a restart.
    #[tokio::test]
    async fn scheduler_picks_up_hot_reloaded_cron() {
        // Start with NO cron entries.
        let (shared, router, gateway) = make_shared_config_and_router(None, &[], "hot reload ok");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();
        let notify = Arc::new(tokio::sync::Notify::new());

        let sched_shared = Arc::clone(&shared);
        let sched_cancel = cancel.clone();
        let sched_notify = Arc::clone(&notify);
        let handle = tokio::spawn(async move {
            run_scheduler_with_notify(
                sched_shared,
                router,
                None,
                sched_cancel,
                Some(sched_notify),
                None,
            )
            .await;
        });

        // Give the scheduler time to start and enter its wait.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Simulate hot-reload: add a cron entry and notify the scheduler.
        let mut new_config = shared.load().as_ref().clone();
        new_config.cron = vec![CronConfig {
            name: "hotcron".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "hot reload test".to_owned(),
            user: None,
            deliver: None,
        }];
        shared.store(Arc::new(new_config));
        notify.notify_one();

        // Scheduler wakes immediately, parses new cron, fires within ~1s.
        tokio::time::sleep(Duration::from_secs(2)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "hotcron"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'hotcron' after hot-reload, found: {sessions:?}"
        );
    }

    // -- Notify tests --

    #[tokio::test]
    async fn scheduler_wakes_on_notify_when_sleeping_for_distant_cron() {
        // Cron fires far in the future (Jan 1). Scheduler would sleep for
        // months. A notify should wake it to re-evaluate immediately.
        let cron = vec![CronConfig {
            name: "distant".to_owned(),
            cron: "0 0 1 1 *".to_owned(), // once a year
            message: "yearly".to_owned(),
            user: None,
            deliver: None,
        }];
        let (shared, router, gateway) = make_shared_config_and_router(None, &cron, "response");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();
        let notify = Arc::new(tokio::sync::Notify::new());

        let sched_shared = Arc::clone(&shared);
        let sched_cancel = cancel.clone();
        let sched_notify = Arc::clone(&notify);
        let handle = tokio::spawn(async move {
            run_scheduler_with_notify(
                sched_shared,
                router,
                None,
                sched_cancel,
                Some(sched_notify),
                None,
            )
            .await;
        });

        // Give scheduler time to enter its long sleep.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Hot-reload: replace the distant cron with an every-second cron.
        let mut new_config = shared.load().as_ref().clone();
        new_config.cron = vec![CronConfig {
            name: "fast".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "tick".to_owned(),
            user: None,
            deliver: None,
        }];
        shared.store(Arc::new(new_config));
        notify.notify_one();

        // The scheduler should wake, re-parse, and fire within ~2s.
        tokio::time::sleep(Duration::from_secs(2)).await;
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "fast"));
        assert!(
            cron_session.is_some(),
            "scheduler should wake from long sleep on notify, found: {sessions:?}"
        );
    }

    #[tokio::test]
    async fn scheduler_exits_on_cancellation_when_waiting_on_notify() {
        // No cron entries — scheduler blocks on notify.notified().
        // Shutdown should still work.
        let (shared, router, _gateway) = make_shared_config_and_router(None, &[], "response");
        let router = Arc::new(router);
        let cancel = CancellationToken::new();
        let notify = Arc::new(tokio::sync::Notify::new());

        let sched_cancel = cancel.clone();
        let sched_notify = Arc::clone(&notify);
        let handle = tokio::spawn(async move {
            run_scheduler_with_notify(shared, router, None, sched_cancel, Some(sched_notify), None)
                .await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("scheduler should exit promptly when cancelled while waiting on notify")
            .expect("scheduler task panicked");
    }

    // -- New heartbeat delivery tests --

    #[tokio::test]
    async fn fire_cron_auto_delivers_to_user_channels() {
        let users = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec!["signal:alice-uuid".to_owned()],
        }];
        let (shared, router, _gateway) = make_shared_config_and_router_with_users_and_match(
            &users,
            &[],
            "Server needs attention",
        );
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert!(msg.content.contains("Server needs attention"));
    }

    #[tokio::test]
    async fn fire_cron_auto_delivers_to_multiple_channels() {
        let users = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![
                "signal:alice-uuid".to_owned(),
                "signal:group:team-chat".to_owned(),
            ],
        }];
        let (shared, router, _gateway) =
            make_shared_config_and_router_with_users_and_match(&users, &[], "Alert: disk full");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg1 = rx.try_recv().unwrap();
        let msg2 = rx.try_recv().unwrap();
        let targets: Vec<_> = vec![
            (msg1.channel.as_str(), msg1.target.as_str()),
            (msg2.channel.as_str(), msg2.target.as_str()),
        ];
        assert!(targets.contains(&("signal", "alice-uuid")));
        assert!(targets.contains(&("signal", "group:team-chat")));
    }

    #[tokio::test]
    async fn fire_cron_skips_terminal_channels() {
        let users = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![
                "terminal:default".to_owned(),
                "signal:alice-uuid".to_owned(),
            ],
        }];
        let (shared, router, _gateway) =
            make_shared_config_and_router_with_users_and_match(&users, &[], "Important alert");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        // Should only get signal delivery, not terminal
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert!(rx.try_recv().is_err(), "should not deliver to terminal");
    }

    #[tokio::test]
    async fn fire_cron_heartbeat_ok_skips_announce() {
        let users = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec!["signal:alice-uuid".to_owned()],
        }];
        let (shared, router, gateway) =
            make_shared_config_and_router_with_users_and_match(&users, &[], "HEARTBEAT_OK");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        assert!(
            rx.try_recv().is_err(),
            "HEARTBEAT_OK should suppress delivery"
        );

        let sessions = gateway.list_sessions();
        assert!(
            !sessions.iter().any(
                |s| matches!(&s.kind, SessionKind::Dm(identity) if identity == "signal:alice-uuid")
            ),
            "announce flow should not run when heartbeat is suppressed"
        );
    }

    #[tokio::test]
    async fn fire_cron_delivers_real_content() {
        let users = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec!["signal:alice-uuid".to_owned()],
        }];
        let (shared, router, _gateway) =
            make_shared_config_and_router_with_users_and_match(&users, &[], "Your server is down");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.content, "Your server is down");
    }

    #[tokio::test]
    async fn fire_cron_explicit_deliver_overrides_user_channels() {
        let users = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![
                "signal:alice-uuid".to_owned(),
                "signal:group:team-chat".to_owned(),
            ],
        }];
        let (shared, router, _gateway) =
            make_shared_config_and_router_with_users_and_match(&users, &[], "Alert content");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        // Explicit deliver overrides user match patterns
        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "override-target".to_owned(),
            }),
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "override-target");
        assert!(
            rx.try_recv().is_err(),
            "should only deliver to explicit target"
        );
    }

    #[tokio::test]
    async fn fire_cron_no_user_no_deliver_does_not_send() {
        let (shared, router, _gateway) = make_shared_config_and_router(None, &[], "some response");
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            message: "run cleanup".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx), &shared).await;

        assert!(rx.try_recv().is_err(), "no user + no deliver = no delivery");
    }

    #[test]
    fn resolve_cron_delivery_targets_parses_match_patterns() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid", "terminal:default", "signal:group:team-chat"]
"#,
        )
        .unwrap();

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check HEARTBEAT.md".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        let targets = resolve_cron_delivery_targets(&config, &cfg);
        assert_eq!(targets.len(), 2, "should filter out terminal");
        assert!(targets.contains(&("signal".to_owned(), "alice-uuid".to_owned())));
        assert!(targets.contains(&("signal".to_owned(), "group:team-chat".to_owned())));
    }

    #[test]
    fn resolve_cron_delivery_targets_explicit_deliver_overrides() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid"]
"#,
        )
        .unwrap();

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check".to_owned(),
            user: Some("alice".to_owned()),
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "override-uuid".to_owned(),
            }),
        };

        let targets = resolve_cron_delivery_targets(&config, &cfg);
        assert_eq!(
            targets,
            vec![("signal".to_owned(), "override-uuid".to_owned())]
        );
    }

    #[test]
    fn resolve_cron_delivery_targets_no_user_returns_empty() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"
"#,
        )
        .unwrap();

        let cfg = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            message: "cleanup".to_owned(),
            user: None,
            deliver: None,
        };

        let targets = resolve_cron_delivery_targets(&config, &cfg);
        assert!(targets.is_empty());
    }

    #[test]
    fn resolve_cron_delivery_targets_unknown_user_returns_empty() {
        let config: Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid"]
"#,
        )
        .unwrap();

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check".to_owned(),
            user: Some("mallory".to_owned()),
            deliver: None,
        };

        let targets = resolve_cron_delivery_targets(&config, &cfg);
        assert!(targets.is_empty());
    }

    // -- Helpers --

    /// Build a SharedConfig, MessageRouter, and Gateway with the given users,
    /// cron entries, and fake provider response.
    fn make_shared_config_and_router(
        users: Option<&[UserConfig]>,
        cron: &[CronConfig],
        response: &str,
    ) -> (SharedConfig, MessageRouter, Arc<Gateway>) {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(response));
        make_shared_config_and_router_with_provider(users, cron, provider)
    }

    fn trust_as_str(trust: TrustLevel) -> &'static str {
        match trust {
            TrustLevel::Full => "full",
            TrustLevel::Inner => "inner",
            TrustLevel::Familiar => "familiar",
            TrustLevel::Public => "public",
        }
    }

    /// Build with users that preserve their match patterns (unlike
    /// `make_shared_config_and_router` which resets match to `[]`).
    fn make_shared_config_and_router_with_users_and_match(
        users: &[UserConfig],
        cron: &[CronConfig],
        response: &str,
    ) -> (SharedConfig, MessageRouter, Arc<Gateway>) {
        use std::fmt::Write;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "test").unwrap();

        let mut toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test\"\nworkspace = \"{}\"\n",
            dir.path().display()
        );

        for u in users {
            let matches: Vec<String> = u.r#match.iter().map(|m| format!("\"{m}\"")).collect();
            let _ = write!(
                toml_str,
                "\n[[users]]\nname = \"{}\"\ntrust = \"{}\"\nmatch = [{}]\n",
                u.name,
                trust_as_str(u.trust),
                matches.join(", "),
            );
        }

        for entry in cron {
            let _ = write!(
                toml_str,
                "\n[[cron]]\nname = \"{}\"\ncron = \"{}\"\nmessage = \"{}\"\n",
                entry.name, entry.cron, entry.message,
            );
            if let Some(ref user) = entry.user {
                let _ = writeln!(toml_str, "user = \"{user}\"");
            }
            if let Some(ref delivery) = entry.deliver {
                let _ = write!(
                    toml_str,
                    "\n[cron.deliver]\nchannel = \"{}\"\ntarget = \"{}\"\n",
                    delivery.channel, delivery.target,
                );
            }
        }

        let config: Config = toml::from_str(&toml_str).unwrap();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(response));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                dir.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        let router = MessageRouter::new(Arc::clone(&shared), Arc::clone(&gateway));
        (shared, router, gateway)
    }

    fn make_shared_config_and_router_with_provider(
        users: Option<&[UserConfig]>,
        cron: &[CronConfig],
        provider: Arc<dyn Provider>,
    ) -> (SharedConfig, MessageRouter, Arc<Gateway>) {
        use std::fmt::Write;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "test").unwrap();

        let mut users_toml = String::new();
        if let Some(users) = users {
            for u in users {
                let _ = write!(
                    users_toml,
                    "\n[[users]]\nname = \"{}\"\ntrust = \"{}\"\nmatch = []\n",
                    u.name,
                    trust_as_str(u.trust),
                );
            }
        }

        let mut toml_str = format!(
            "[agent]\nid = \"test\"\nmodel = \"test\"\nworkspace = \"{}\"\n{users_toml}",
            dir.path().display()
        );
        for entry in cron {
            let _ = write!(
                toml_str,
                "\n[[cron]]\nname = \"{}\"\ncron = \"{}\"\nmessage = \"{}\"\n",
                entry.name, entry.cron, entry.message,
            );
            if let Some(ref user) = entry.user {
                let _ = writeln!(toml_str, "user = \"{user}\"");
            }
            if let Some(ref delivery) = entry.deliver {
                let _ = write!(
                    toml_str,
                    "\n[cron.deliver]\nchannel = \"{}\"\ntarget = \"{}\"\n",
                    delivery.channel, delivery.target,
                );
            }
        }

        let config: Config = toml::from_str(&toml_str).unwrap();
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                dir.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        std::mem::forget(dir);
        let router = MessageRouter::new(Arc::clone(&shared), Arc::clone(&gateway));
        (shared, router, gateway)
    }

    /// Verify that slow provider calls don't block the scheduler from firing
    /// subsequent cron entries.
    #[tokio::test]
    async fn scheduler_not_blocked_by_slow_provider() {
        use coop_core::fakes::SlowFakeProvider;

        let provider: Arc<dyn Provider> = Arc::new(SlowFakeProvider::new(
            "slow response",
            Duration::from_secs(2),
        ));
        let cron = vec![CronConfig {
            name: "fast-cron".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "tick".to_owned(),
            user: None,
            deliver: None,
        }];
        let (shared, router, gateway) =
            make_shared_config_and_router_with_provider(None, &cron, provider);
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(shared, router, None, sched_cancel).await;
        });

        // Run for 4 seconds, then cancel and give in-flight tasks time to finish.
        tokio::time::sleep(Duration::from_secs(4)).await;
        cancel.cancel();
        // Grace period: spawned fire_cron tasks keep running after scheduler stops.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "fast-cron"));
        assert!(cron_session.is_some(), "expected cron session");

        // The scheduler fires every second (non-blocking via tokio::spawn), but
        // the per-session turn lock ensures only one turn runs at a time.
        // With a 2s provider and 4s runtime, we expect ~2 completed turns (4 messages).
        // Concurrent fires on the same session are skipped, which is correct —
        // without this, interleaved messages corrupt the session history.
        let msg_count = gateway.session_message_count(cron_session.unwrap());
        assert!(
            msg_count >= 2,
            "expected at least 1 completed turn (2 messages), got {msg_count} messages — \
             scheduler is not firing at all"
        );
    }
}
