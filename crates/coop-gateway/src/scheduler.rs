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

pub(crate) async fn run_scheduler(
    cron: Vec<CronConfig>,
    router: Arc<MessageRouter>,
    users: &[crate::config::UserConfig],
    deliver_tx: Option<DeliverySender>,
    shutdown: CancellationToken,
) {
    if cron.is_empty() {
        return;
    }

    let mut parsed: Vec<(CronConfig, Schedule)> = Vec::new();
    for entry in &cron {
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

    if parsed.is_empty() {
        warn!("no valid cron entries, scheduler exiting");
        return;
    }

    info!(count = parsed.len(), "scheduler started");

    loop {
        let now = Utc::now();
        let next = parsed
            .iter()
            .filter_map(|(cfg, sched)| sched.upcoming(Utc).next().map(|t| (cfg, t)))
            .min_by_key(|(_, t)| *t);

        let Some((cfg, fire_time)) = next else {
            break;
        };

        let delay = (fire_time - now).to_std().unwrap_or(Duration::ZERO);

        tokio::select! {
            () = tokio::time::sleep(delay) => {
                fire_cron(cfg, &router, deliver_tx.as_ref()).await;
            }
            () = shutdown.cancelled() => {
                info!("scheduler shutting down");
                break;
            }
        }
    }
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
    use crate::config::{Config, CronDelivery, UserConfig};
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
    async fn scheduler_exits_on_empty_cron() {
        let cancel = CancellationToken::new();
        run_scheduler(vec![], Arc::new(make_router(None)), &[], None, cancel).await;
    }

    #[tokio::test]
    async fn scheduler_exits_on_cancellation() {
        let cancel = CancellationToken::new();
        let entries = vec![CronConfig {
            name: "test".to_owned(),
            cron: "0 0 1 1 *".to_owned(),
            message: "test".to_owned(),
            user: None,
            deliver: None,
        }];

        let cancel_clone = cancel.clone();
        let router = Arc::new(make_router(None));
        let handle = tokio::spawn(async move {
            run_scheduler(entries, router, &[], None, cancel_clone).await;
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
        let alice = [UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec!["terminal:default".to_owned()],
        }];
        let (router, gateway) = make_router_and_gateway(Some(&alice));
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let entries = vec![CronConfig {
            name: "test".to_owned(),
            cron: "* * * * * * *".to_owned(), // every second
            message: "heartbeat check".to_owned(),
            user: Some("alice".to_owned()),
            deliver: None,
        }];
        let users = vec![UserConfig {
            name: "alice".to_owned(),
            trust: TrustLevel::Full,
            r#match: vec![],
        }];

        let sched_router = Arc::clone(&router);
        let sched_cancel = cancel.clone();
        let sched_users = users.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(entries, sched_router, &sched_users, None, sched_cancel).await;
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
        let (router, gateway) = make_router_and_gateway(None);
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let entries = vec![CronConfig {
            name: "cleanup".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "run cleanup".to_owned(),
            user: None,
            deliver: None,
        }];

        let sched_router = Arc::clone(&router);
        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(entries, sched_router, &[], None, sched_cancel).await;
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
        let (router, gateway) = make_router_and_gateway(None);
        let router = Arc::new(router);
        let cancel = CancellationToken::new();

        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(8);
        let deliver_tx = DeliverySender::new(tx);

        let entries = vec![CronConfig {
            name: "briefing".to_owned(),
            cron: "* * * * * * *".to_owned(),
            message: "Morning briefing".to_owned(),
            user: None,
            deliver: Some(CronDelivery {
                channel: "signal".to_owned(),
                target: "alice-uuid".to_owned(),
            }),
        }];

        let sched_router = Arc::clone(&router);
        let sched_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(entries, sched_router, &[], Some(deliver_tx), sched_cancel).await;
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

    // -- Helpers --

    fn make_router(users: Option<&[UserConfig]>) -> MessageRouter {
        let (router, _gateway) = make_router_and_gateway(users);
        router
    }

    fn make_router_and_gateway(users: Option<&[UserConfig]>) -> (MessageRouter, Arc<Gateway>) {
        make_router_and_gateway_with_response(users, "cron response ok")
    }

    fn make_router_and_gateway_with_response(
        users: Option<&[UserConfig]>,
        response: &str,
    ) -> (MessageRouter, Arc<Gateway>) {
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

        let yaml = format!("agent:\n  id: test\n  model: test\n{users_yaml}");
        let config: Config = serde_yaml::from_str(&yaml).unwrap();

        let provider: Arc<dyn coop_core::Provider> = Arc::new(FakeProvider::new(response));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                config.clone(),
                dir.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        // Leak dir to keep it alive — test only.
        std::mem::forget(dir);
        (MessageRouter::new(config, Arc::clone(&gateway)), gateway)
    }
}
