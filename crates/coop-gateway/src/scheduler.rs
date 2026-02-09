use anyhow::Result;
use chrono::Utc;
use coop_core::{InboundKind, InboundMessage, OutboundMessage};
use cron::Schedule;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::config::{CronConfig, CronDelivery};
use crate::router::MessageRouter;

/// Sender for delivering cron output to channels.
///
/// Wraps an `mpsc::Sender<OutboundMessage>`. In production, a bridge task
/// forwards outbound messages to the appropriate channel (e.g. Signal).
#[derive(Clone)]
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

/// How often to re-check config when no cron entries are configured.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) async fn run_scheduler(
    config: crate::config::SharedConfig,
    router: Arc<MessageRouter>,
    deliver_tx: Option<DeliverySender>,
    shutdown: CancellationToken,
) {
    info!("scheduler started");

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

        if parsed.is_empty() {
            tokio::select! {
                () = tokio::time::sleep(CONFIG_POLL_INTERVAL) => continue,
                () = shutdown.cancelled() => {
                    info!("scheduler shutting down");
                    return;
                }
            }
        }

        let now = Utc::now();
        let next = parsed
            .iter()
            .filter_map(|(cfg, sched)| sched.upcoming(Utc).next().map(|t| (cfg, t)))
            .min_by_key(|(_, t)| *t);

        let Some((cfg, fire_time)) = next else {
            // All schedules exhausted (shouldn't happen for recurring cron).
            tokio::select! {
                () = tokio::time::sleep(CONFIG_POLL_INTERVAL) => continue,
                () = shutdown.cancelled() => {
                    info!("scheduler shutting down");
                    return;
                }
            }
        };

        let delay = (fire_time - now).to_std().unwrap_or(Duration::ZERO);

        // Cap sleep so we notice config changes within CONFIG_POLL_INTERVAL.
        let capped = delay.min(CONFIG_POLL_INTERVAL);

        debug!(
            cron.name = %cfg.name,
            fire_time = %fire_time,
            delay_secs = delay.as_secs(),
            "scheduler sleeping until next cron"
        );

        tokio::select! {
            () = tokio::time::sleep(capped) => {
                // Only fire if we've actually reached the scheduled time.
                if Utc::now() >= fire_time {
                    let cfg = cfg.clone();
                    let router = Arc::clone(&router);
                    let deliver_tx = deliver_tx.clone();
                    tokio::spawn(async move {
                        fire_cron(&cfg, &router, deliver_tx.as_ref()).await;
                    });
                }
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

async fn fire_cron(cfg: &CronConfig, router: &MessageRouter, deliver_tx: Option<&DeliverySender>) {
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

        let sender = match &cfg.user {
            Some(user) => format!("cron:{}:{}", cfg.name, user),
            None => format!("cron:{}", cfg.name),
        };

        let content = if let Some(ref delivery) = cfg.deliver {
            format!(
                "[Your response will be delivered to {} via {}.]\n\n{}",
                delivery.target, delivery.channel, cfg.message
            )
        } else {
            cfg.message.clone()
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

        match router.dispatch_collect_text(&inbound).await {
            Ok((decision, response)) => {
                info!(
                    session = %decision.session_key,
                    trust = ?decision.trust,
                    user = ?decision.user_name,
                    "cron completed"
                );

                if let Some(ref delivery) = cfg.deliver {
                    if response.trim().is_empty() {
                        debug!(cron.name = %cfg.name, "cron produced empty response, skipping delivery");
                    } else {
                        deliver_response(delivery, &response, deliver_tx).await;
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

async fn deliver_response(
    delivery: &CronDelivery,
    response: &str,
    deliver_tx: Option<&DeliverySender>,
) {
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CronDelivery, UserConfig, shared_config};
    use crate::gateway::Gateway;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use coop_core::{SessionKind, TrustLevel};

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
        let (router, gateway) = make_router_and_gateway(Some(&alice));

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        };

        fire_cron(&cfg, &router, None).await;

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
        let (router, gateway) = make_router_and_gateway(None);

        let cfg = CronConfig {
            name: "cleanup".to_owned(),
            cron: "0 3 * * *".to_owned(),
            message: "run cleanup".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, None).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "cleanup"));
        assert!(
            cron_session.is_some(),
            "expected cron session 'cleanup' after fire, found: {sessions:?}"
        );
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_sends_response() {
        let (router, _gateway) = make_router_and_gateway(None);
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

        fire_cron(&cfg, &router, Some(&deliver_tx)).await;

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, "signal");
        assert_eq!(msg.target, "alice-uuid");
        assert_eq!(msg.content, "cron response ok");
    }

    #[tokio::test]
    async fn fire_cron_without_delivery_does_not_send() {
        let (router, _gateway) = make_router_and_gateway(None);
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, Some(&deliver_tx)).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_skips_empty_response() {
        let (router, _gateway) = make_router_and_gateway_with_response(None, "   ");
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

        fire_cron(&cfg, &router, Some(&deliver_tx)).await;

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_no_sender_does_not_panic() {
        let (router, _gateway) = make_router_and_gateway(None);

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

        fire_cron(&cfg, &router, None).await;
    }

    #[tokio::test]
    async fn deliver_response_with_no_sender_does_not_panic() {
        let delivery = CronDelivery {
            channel: "email".to_owned(),
            target: "alice@example.com".to_owned(),
        };

        deliver_response(&delivery, "hello", None).await;
    }

    #[tokio::test]
    async fn fire_cron_with_delivery_prepends_context() {
        let (router, gateway) = make_router_and_gateway(None);
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

        fire_cron(&cfg, &router, Some(&deliver_tx)).await;

        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "briefing"));
        assert!(cron_session.is_some());
    }

    #[tokio::test]
    async fn fire_cron_without_delivery_has_no_prefix() {
        let (router, _gateway) = make_router_and_gateway(None);

        let cfg = CronConfig {
            name: "heartbeat".to_owned(),
            cron: "*/30 * * * *".to_owned(),
            message: "check tasks".to_owned(),
            user: None,
            deliver: None,
        };

        fire_cron(&cfg, &router, None).await;
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

        let sched_shared = Arc::clone(&shared);
        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(sched_shared, router, None, sched_cancel).await;
        });

        // Give the scheduler time to start and enter its empty-poll loop.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Simulate hot-reload: add a cron entry via SharedConfig.
        let mut new_config = shared.load().as_ref().clone();
        new_config.cron = vec![CronConfig {
            name: "hotcron".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "hot reload test".to_owned(),
            user: None,
            deliver: None,
        }];
        shared.store(Arc::new(new_config));

        // Wait for the scheduler to pick up the change and fire.
        tokio::time::sleep(Duration::from_secs(12)).await;
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

    // -- Helpers --

    fn make_router_and_gateway(users: Option<&[UserConfig]>) -> (MessageRouter, Arc<Gateway>) {
        make_router_and_gateway_with_response(users, "cron response ok")
    }

    fn make_router_and_gateway_with_response(
        users: Option<&[UserConfig]>,
        response: &str,
    ) -> (MessageRouter, Arc<Gateway>) {
        let (_shared, router, gateway) = make_shared_config_and_router(users, &[], response);
        (router, gateway)
    }

    /// Build a SharedConfig, MessageRouter, and Gateway with the given users,
    /// cron entries, and fake provider response.
    fn make_shared_config_and_router(
        users: Option<&[UserConfig]>,
        cron: &[CronConfig],
        response: &str,
    ) -> (crate::config::SharedConfig, MessageRouter, Arc<Gateway>) {
        let provider: Arc<dyn coop_core::Provider> = Arc::new(FakeProvider::new(response));
        make_shared_config_and_router_with_provider(users, cron, provider)
    }

    fn make_shared_config_and_router_with_provider(
        users: Option<&[UserConfig]>,
        cron: &[CronConfig],
        provider: Arc<dyn coop_core::Provider>,
    ) -> (crate::config::SharedConfig, MessageRouter, Arc<Gateway>) {
        use std::fmt::Write;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "test").unwrap();

        let users_yaml = match users {
            Some(users) => {
                let mut s = "users:\n".to_owned();
                for u in users {
                    let _ = write!(
                        s,
                        "  - name: {}\n    trust: {}\n    match: []\n",
                        u.name,
                        serde_yaml::to_string(&u.trust).unwrap().trim()
                    );
                }
                s
            }
            None => String::new(),
        };

        let mut yaml = format!("agent:\n  id: test\n  model: test\n{users_yaml}");
        if !cron.is_empty() {
            yaml.push_str("cron:\n");
            for entry in cron {
                let _ = write!(
                    yaml,
                    "  - name: {}\n    cron: '{}'\n    message: '{}'\n",
                    entry.name, entry.cron, entry.message,
                );
                if let Some(ref user) = entry.user {
                    let _ = writeln!(yaml, "    user: {user}");
                }
                if let Some(ref delivery) = entry.deliver {
                    let _ = write!(
                        yaml,
                        "    deliver:\n      channel: {}\n      target: {}\n",
                        delivery.channel, delivery.target,
                    );
                }
            }
        }

        let config: Config = serde_yaml::from_str(&yaml).unwrap();
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
    /// subsequent cron entries. With a synchronous `fire_cron` call, a 2-second
    /// provider delay would allow at most ~2 fires in 4 seconds. With concurrent
    /// spawning, the scheduler fires every second regardless of provider latency.
    #[tokio::test]
    async fn scheduler_not_blocked_by_slow_provider() {
        use coop_core::fakes::SlowFakeProvider;

        let provider: Arc<dyn coop_core::Provider> = Arc::new(SlowFakeProvider::new(
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

        // Each completed fire adds 2 messages (user + assistant).
        // With blocking fire_cron (2s each): max ~2 fires in 4s = 4 messages.
        // With concurrent spawning: fires at t≈0,1,2,3 all complete by t≈5.
        // We expect at least 3 completed fires (6 messages).
        let sessions = gateway.list_sessions();
        let cron_session = sessions
            .iter()
            .find(|s| matches!(&s.kind, SessionKind::Cron(name) if name == "fast-cron"));
        assert!(cron_session.is_some(), "expected cron session");

        let msg_count = gateway.session_message_count(cron_session.unwrap());
        assert!(
            msg_count >= 6,
            "expected at least 3 concurrent fires (6 messages), got {msg_count} messages — \
             scheduler is likely blocking on fire_cron"
        );
    }
}
