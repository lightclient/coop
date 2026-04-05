use anyhow::Result;
use chrono::Utc;
use chrono_tz::Tz;
use coop_core::{InboundKind, InboundMessage, Message, OutboundMessage, SessionKey, SessionKind};
use std::path::Path;
use tokio::sync::{mpsc, oneshot};
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::config::{Config, CronConfig, CronDeliveryMode, SharedConfig};
use crate::cron_timezone::resolve_cron_timezone;
use crate::heartbeat::{
    NO_ACTION_NEEDED_TOKEN, SuppressionTokenResult, contains_legacy_heartbeat_token,
    is_heartbeat_content_empty, strip_suppression_token,
};
use crate::router::{MessageRouter, RouteDecision};

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

pub(crate) enum CronCommand {
    RunNow {
        name: String,
        deliver: bool,
        origin_session_id: String,
        reply: oneshot::Sender<Result<CronTriggerResult>>,
    },
}

pub(crate) type CronCommandSender = mpsc::Sender<CronCommand>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CronTriggerStatus {
    Completed,
    CompletedEmpty,
    Suppressed,
    SkippedHeartbeat,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CronTriggerResult {
    pub cron_name: String,
    pub status: CronTriggerStatus,
    pub response: Option<String>,
    pub delivered_to: usize,
    pub attempted_to: usize,
}

impl CronTriggerResult {
    fn completed(
        cron_name: &str,
        response: String,
        delivered_to: usize,
        attempted_to: usize,
    ) -> Self {
        Self {
            cron_name: cron_name.to_owned(),
            status: CronTriggerStatus::Completed,
            response: Some(response),
            delivered_to,
            attempted_to,
        }
    }

    fn completed_empty(cron_name: &str) -> Self {
        Self {
            cron_name: cron_name.to_owned(),
            status: CronTriggerStatus::CompletedEmpty,
            response: None,
            delivered_to: 0,
            attempted_to: 0,
        }
    }

    fn suppressed(cron_name: &str) -> Self {
        Self {
            cron_name: cron_name.to_owned(),
            status: CronTriggerStatus::Suppressed,
            response: None,
            delivered_to: 0,
            attempted_to: 0,
        }
    }

    fn skipped_heartbeat(cron_name: &str) -> Self {
        Self {
            cron_name: cron_name.to_owned(),
            status: CronTriggerStatus::SkippedHeartbeat,
            response: None,
            delivered_to: 0,
            attempted_to: 0,
        }
    }

    fn busy(cron_name: &str) -> Self {
        Self {
            cron_name: cron_name.to_owned(),
            status: CronTriggerStatus::Busy,
            response: None,
            delivered_to: 0,
            attempted_to: 0,
        }
    }
}

enum CronRunTrigger {
    Scheduled,
    Manual {
        deliver: bool,
        origin_session_id: String,
    },
}

impl CronRunTrigger {
    fn kind(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual { .. } => "manual",
        }
    }

    fn delivery_enabled(&self) -> bool {
        match self {
            Self::Scheduled => true,
            Self::Manual { deliver, .. } => *deliver,
        }
    }

    fn origin_session_id(&self) -> Option<&str> {
        match self {
            Self::Scheduled => None,
            Self::Manual {
                origin_session_id, ..
            } => Some(origin_session_id.as_str()),
        }
    }
}

pub(crate) async fn run_scheduled_cron(
    cfg: &CronConfig,
    timezone: Tz,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
) -> Result<CronTriggerResult> {
    run_cron_once(
        cfg,
        timezone,
        router,
        deliver_tx,
        shared_config,
        CronRunTrigger::Scheduled,
    )
    .await
}

pub(crate) async fn run_manual_cron_by_name(
    name: &str,
    deliver: bool,
    origin_session_id: &str,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
) -> Result<CronTriggerResult> {
    let config_snapshot = shared_config.load();
    let matching: Vec<CronConfig> = config_snapshot
        .cron
        .iter()
        .filter(|cron| cron.name.as_str() == name)
        .cloned()
        .collect();

    let cfg = match matching.as_slice() {
        [] => anyhow::bail!("unknown cron: {name}"),
        [cfg] => cfg.clone(),
        _ => anyhow::bail!("cron name '{name}' is not unique"),
    };

    let timezone = resolve_cron_timezone(&cfg, &config_snapshot.users)?;
    drop(config_snapshot);

    run_cron_once(
        &cfg,
        timezone,
        router,
        deliver_tx,
        shared_config,
        CronRunTrigger::Manual {
            deliver,
            origin_session_id: origin_session_id.to_owned(),
        },
    )
    .await
}

fn cron_session_key(agent_id: &str, cron_name: &str) -> SessionKey {
    SessionKey {
        agent_id: agent_id.to_owned(),
        kind: SessionKind::Cron(cron_name.to_owned()),
    }
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

/// Wait for any in-progress turns on delivery target DM sessions to complete.
///
/// When a cron fires while a user has tool calls in flight, running the cron
/// turn concurrently can compete for provider capacity and the subsequent
/// `announce_to_session` injection can corrupt the `tool_use`/`tool_result`
/// pairing. This function acquires (and immediately releases) each target's
/// turn lock, which blocks until any active turn finishes.
///
/// Groups are excluded — they use direct delivery without session injection.
async fn wait_for_delivery_sessions(
    delivery_targets: &[(String, String)],
    agent_id: &str,
    router: &MessageRouter,
    origin_session_id: Option<&str>,
) {
    for (channel, target) in delivery_targets {
        if target.starts_with("group:") {
            continue;
        }

        let dm_session_key = SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Dm(format!("{channel}:{target}")),
        };
        let dm_session_id = dm_session_key.to_string();

        if origin_session_id.is_some_and(|origin| origin == dm_session_id) {
            debug!(
                session = %dm_session_key,
                "cron delivery target is the origin session; skipping wait to avoid deadlock"
            );
            continue;
        }

        if router.has_active_turn(&dm_session_key) {
            info!(
                session = %dm_session_key,
                "cron waiting for active turn to complete on delivery target"
            );
            let lock = router.session_turn_lock(&dm_session_key);
            let _guard = lock.lock().await;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_cron_once(
    cfg: &CronConfig,
    timezone: Tz,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    shared_config: &SharedConfig,
    trigger: CronRunTrigger,
) -> Result<CronTriggerResult> {
    let delivery_mode = cfg.effective_delivery_mode();
    let trigger_kind = trigger.kind();
    let delivery_enabled = trigger.delivery_enabled();
    let origin_session_id = trigger.origin_session_id().map(str::to_owned);

    let span = if trigger_kind == "scheduled" {
        info_span!(
            "cron_fired",
            cron.name = %cfg.name,
            cron.timezone = %timezone,
            cron.trigger = %trigger_kind,
            cron.delivery_mode = %delivery_mode,
            cron.deliver = delivery_enabled,
            cron.legacy_delivery_mode = cfg.uses_legacy_delivery_mode(),
            user = ?cfg.user,
        )
    } else {
        info_span!(
            "cron_triggered",
            cron.name = %cfg.name,
            cron.timezone = %timezone,
            cron.trigger = %trigger_kind,
            cron.delivery_mode = %delivery_mode,
            cron.deliver = delivery_enabled,
            cron.legacy_delivery_mode = cfg.uses_legacy_delivery_mode(),
            user = ?cfg.user,
            origin.session = ?origin_session_id,
        )
    };

    async {
        let config_snapshot = shared_config.load();
        let agent_id = config_snapshot.agent.id.clone();
        let session_key = cron_session_key(&agent_id, &cfg.name);

        info!(
            cron = %cfg.cron,
            cron.timezone = %timezone,
            message = %cfg.message,
            user = ?cfg.user,
            cron.trigger = %trigger_kind,
            cron.delivery_mode = %delivery_mode,
            cron.deliver = delivery_enabled,
            "cron firing"
        );

        if router.has_active_turn(&session_key) {
            info!(session = %session_key, "cron run skipped: session already active");
            return Ok(CronTriggerResult::busy(&cfg.name));
        }

        let delivery_targets = if delivery_enabled {
            resolve_cron_delivery_targets(&config_snapshot, cfg)
        } else {
            Vec::new()
        };

        if should_skip_heartbeat(&config_snapshot, &cfg.message) {
            debug!(cron.name = %cfg.name, "heartbeat skipped: empty heartbeat file");
            return Ok(CronTriggerResult::skipped_heartbeat(&cfg.name));
        }

        wait_for_delivery_sessions(
            &delivery_targets,
            &agent_id,
            router,
            origin_session_id.as_deref(),
        )
        .await;

        let sender = match &cfg.user {
            Some(user) => format!("cron:{}:{}", cfg.name, user),
            None => format!("cron:{}", cfg.name),
        };

        let prompt_channel = delivery_targets
            .first()
            .map(|(channel, _)| channel.clone());

        let content = if delivery_targets.is_empty() {
            cfg.message.clone()
        } else {
            format!(
                "[Your response will be delivered to the user via {}. Your response is delivered automatically.]\n\n{}",
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
            group_revision: None,
        };

        let prompt_delivery_mode = (!delivery_targets.is_empty()).then_some(delivery_mode);
        let delivery_prompt_channel = prompt_channel.clone();

        let (decision, response) = router
            .dispatch_collect_text_with_channel_and_cron_delivery(
                &inbound,
                prompt_channel,
                prompt_delivery_mode,
            )
            .await?;

        info!(
            session = %decision.session_key,
            trust = ?decision.trust,
            user = ?decision.user_name,
            cron.trigger = %trigger_kind,
            "cron completed"
        );

        if response.trim().is_empty() {
            let status = if router.has_active_turn(&session_key) {
                CronTriggerResult::busy(&cfg.name)
            } else {
                debug!(cron.name = %cfg.name, "cron produced empty response");
                CronTriggerResult::completed_empty(&cfg.name)
            };
            return Ok(status);
        }

        deliver_cron_response(
            cfg,
            response,
            &delivery_targets,
            &decision,
            delivery_prompt_channel.as_deref(),
            &agent_id,
            router,
            deliver_tx,
            origin_session_id.as_deref(),
        )
        .await
    }
    .instrument(span)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn deliver_cron_response(
    cfg: &CronConfig,
    response: String,
    delivery_targets: &[(String, String)],
    decision: &RouteDecision,
    prompt_channel: Option<&str>,
    agent_id: &str,
    router: &MessageRouter,
    deliver_tx: Option<&DeliverySender>,
    origin_session_id: Option<&str>,
) -> Result<CronTriggerResult> {
    if delivery_targets.is_empty() {
        return Ok(CronTriggerResult::completed(&cfg.name, response, 0, 0));
    }

    let delivery_mode = cfg.effective_delivery_mode();
    let content_to_deliver = match delivery_mode {
        CronDeliveryMode::AsNeeded => match strip_suppression_token(&response) {
            SuppressionTokenResult::Suppress => {
                if contains_legacy_heartbeat_token(&response) {
                    debug!(
                        cron.name = %cfg.name,
                        cron.delivery_mode = %delivery_mode,
                        token = %NO_ACTION_NEEDED_TOKEN,
                        legacy_token = true,
                        "cron suppressed: legacy HEARTBEAT_OK token detected"
                    );
                } else {
                    debug!(
                        cron.name = %cfg.name,
                        cron.delivery_mode = %delivery_mode,
                        token = %NO_ACTION_NEEDED_TOKEN,
                        "cron suppressed: NO_ACTION_NEEDED token detected"
                    );
                }
                return Ok(CronTriggerResult::suppressed(&cfg.name));
            }
            SuppressionTokenResult::Deliver(cleaned) => match router
                .review_as_needed_cron_delivery(
                    decision,
                    &cfg.message,
                    cfg.review_prompt.as_deref(),
                    prompt_channel,
                    &cleaned,
                )
                .await
            {
                Ok(true) => cleaned,
                Ok(false) => {
                    debug!(
                        cron.name = %cfg.name,
                        cron.delivery_mode = %delivery_mode,
                        "cron suppressed: as-needed delivery review vetoed message"
                    );
                    return Ok(CronTriggerResult::suppressed(&cfg.name));
                }
                Err(error) => {
                    warn!(
                        cron.name = %cfg.name,
                        cron.delivery_mode = %delivery_mode,
                        fallback = "deliver_original",
                        error = %error,
                        "cron delivery review failed; delivering original as-needed content"
                    );
                    cleaned
                }
            },
        },
        CronDeliveryMode::Always => {
            if matches!(
                strip_suppression_token(&response),
                SuppressionTokenResult::Suppress
            ) {
                warn!(
                    cron.name = %cfg.name,
                    cron.delivery_mode = %delivery_mode,
                    token = %NO_ACTION_NEEDED_TOKEN,
                    "cron configured for always delivery emitted suppression token; delivering literal content"
                );
            }
            response.clone()
        }
    };

    let mut delivered_to = 0;
    for (channel, target) in delivery_targets {
        if announce_to_session(
            &cfg.name,
            &cfg.message,
            &content_to_deliver,
            channel,
            target,
            agent_id,
            router,
            deliver_tx,
            origin_session_id,
        )
        .await
        {
            delivered_to += 1;
        }
    }

    Ok(CronTriggerResult::completed(
        &cfg.name,
        content_to_deliver,
        delivered_to,
        delivery_targets.len(),
    ))
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
    origin_session_id: Option<&str>,
) -> bool {
    if target.starts_with("group:") {
        return deliver_to_target(channel, target, cron_output, deliver_tx).await;
    }

    let dm_session_key = SessionKey {
        agent_id: agent_id.to_owned(),
        kind: SessionKind::Dm(format!("{channel}:{target}")),
    };
    let dm_session_id = dm_session_key.to_string();

    if origin_session_id.is_some_and(|origin| origin == dm_session_id) {
        debug!(
            session = %dm_session_key,
            "cron delivery target is the origin session; skipping session injection"
        );
        return deliver_to_target(channel, target, cron_output, deliver_tx).await;
    }

    let span = info_span!(
        "cron_announce",
        cron.name = %cron_name,
        channel = %channel,
        target = %target,
        session = %dm_session_key,
    );

    async {
        let lock = router.session_turn_lock(&dm_session_key);
        let _guard = lock.lock().await;

        debug!(
            dm_session = %dm_session_key,
            "injecting cron output into DM session"
        );

        let context_msg =
            Message::user().with_text(format!("[Scheduled: {cron_name}]\n{cron_message}"));
        router.append_to_session(&dm_session_key, context_msg);

        let output_msg = Message::assistant().with_text(cron_output);
        router.append_to_session(&dm_session_key, output_msg);

        deliver_to_target(channel, target, cron_output, deliver_tx).await
    }
    .instrument(span)
    .await
}

/// Check if the cron message references HEARTBEAT.md and if that file is empty.
fn should_skip_heartbeat(config: &Config, message: &str) -> bool {
    if !message.contains("HEARTBEAT.md") {
        return false;
    }

    let workspace_str = &config.agent.workspace;
    let workspace = std::path::PathBuf::from(workspace_str);
    if !workspace.is_absolute() {
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
        Ok(content) => is_heartbeat_content_empty(&content),
        Err(_) => false,
    }
}

pub(crate) async fn deliver_to_target(
    channel: &str,
    target: &str,
    content: &str,
    deliver_tx: Option<&DeliverySender>,
) -> bool {
    let Some(tx) = deliver_tx else {
        warn!(
            channel = %channel,
            target = %target,
            "delivery target resolved but no delivery sender available"
        );
        return false;
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
                true
            }
            Err(e) => {
                error!(error = %e, "cron delivery failed");
                false
            }
        }
    }
    .instrument(span)
    .await
}
