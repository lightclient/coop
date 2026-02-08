use anyhow::Result;
use coop_core::{InboundMessage, SessionKey, SessionKind, TrustLevel, TurnEvent};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info_span};

use std::sync::Arc;

use crate::config::Config;
use crate::gateway::Gateway;
use crate::trust::resolve_trust;

#[derive(Debug, Clone)]
pub(crate) struct RouteDecision {
    pub session_key: SessionKey,
    pub trust: TrustLevel,
}

#[derive(Clone)]
pub(crate) struct MessageRouter {
    config: Config,
    gateway: Arc<Gateway>,
}

impl MessageRouter {
    pub(crate) fn new(config: Config, gateway: Arc<Gateway>) -> Self {
        Self { config, gateway }
    }

    pub(crate) fn route(&self, msg: &InboundMessage) -> RouteDecision {
        route_message(msg, &self.config)
    }

    pub(crate) async fn dispatch(
        &self,
        msg: &InboundMessage,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<RouteDecision> {
        let decision = self.route(msg);
        let span = info_span!(
            "route_message",
            session = %decision.session_key,
            trust = ?decision.trust,
            source = %msg.channel,
        );
        debug!(parent: &span, sender = %msg.sender, "routing message");
        self.gateway
            .run_turn_with_trust(
                &decision.session_key,
                &msg.content,
                decision.trust,
                event_tx,
            )
            .instrument(span)
            .await?;
        Ok(decision)
    }

    #[allow(dead_code)]
    pub(crate) async fn dispatch_collect_text(
        &self,
        msg: &InboundMessage,
    ) -> Result<(RouteDecision, String)> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let router = self.clone();
        let message = msg.clone();

        let dispatch_task = tokio::spawn(async move { router.dispatch(&message, event_tx).await });

        let mut text = String::new();
        let mut fallback_assistant = String::new();

        while let Some(event) = event_rx.recv().await {
            match event {
                TurnEvent::TextDelta(delta) => {
                    text.push_str(&delta);
                }
                TurnEvent::AssistantMessage(message) => {
                    if fallback_assistant.is_empty() {
                        fallback_assistant = message.text();
                    }
                }
                TurnEvent::Error(message) => {
                    return Err(anyhow::anyhow!(message));
                }
                TurnEvent::Done(_) => {
                    break;
                }
                TurnEvent::ToolStart { .. } | TurnEvent::ToolResult { .. } => {}
            }
        }

        let decision = match dispatch_task.await {
            Ok(result) => result?,
            Err(error) => anyhow::bail!("router task failed: {error}"),
        };

        if text.is_empty() {
            text = fallback_assistant;
        }

        Ok((decision, text))
    }
}

pub(crate) fn route_message(msg: &InboundMessage, config: &Config) -> RouteDecision {
    let agent_id = config.agent.id.clone();
    let identity = format!("{}:{}", msg.channel, msg.sender);

    let explicit_kind = if msg.channel == "terminal:default" {
        msg.reply_to
            .as_deref()
            .and_then(|session| parse_explicit_session_kind(session, &agent_id))
    } else {
        None
    };

    let user_trust = config
        .users
        .iter()
        .find(|user| {
            user.r#match.iter().any(|pattern| {
                pattern == &identity || pattern == &msg.channel || pattern == &msg.sender
            })
        })
        .map_or(TrustLevel::Public, |user| user.trust);

    let group_context = msg.is_group
        || explicit_kind
            .as_ref()
            .is_some_and(|kind| matches!(kind, SessionKind::Group(_)));

    let ceiling = if group_context {
        TrustLevel::Familiar
    } else {
        TrustLevel::Full
    };
    let trust = resolve_trust(user_trust, ceiling);

    let kind = if let Some(kind) = explicit_kind {
        kind
    } else if msg.is_group {
        let group_id = msg.chat_id.clone().unwrap_or_else(|| msg.channel.clone());
        let namespaced_group = if group_id.starts_with(&format!("{}:", msg.channel)) {
            group_id
        } else {
            format!("{}:{group_id}", msg.channel)
        };
        SessionKind::Group(namespaced_group)
    } else {
        match msg.channel.as_str() {
            "terminal:default" => SessionKind::Main,
            _ => SessionKind::Dm(identity),
        }
    };

    RouteDecision {
        session_key: SessionKey { agent_id, kind },
        trust,
    }
}

fn parse_explicit_session_kind(session: &str, agent_id: &str) -> Option<SessionKind> {
    if session == "main" {
        return Some(SessionKind::Main);
    }

    let rest = session.strip_prefix(&format!("{agent_id}:"))?;

    if rest == "main" {
        return Some(SessionKind::Main);
    }

    if let Some(dm) = rest.strip_prefix("dm:") {
        return Some(SessionKind::Dm(dm.to_string()));
    }

    if let Some(group) = rest.strip_prefix("group:") {
        return Some(SessionKind::Group(group.to_string()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use coop_core::Provider;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn test_config() -> Config {
        serde_yaml::from_str(
            r"
agent:
  id: reid
  model: test
users:
  - name: alice
    trust: full
    match: ['terminal:default', 'signal:alice-uuid']
  - name: bob
    trust: inner
    match: ['signal:bob-uuid']
",
        )
        .unwrap()
    }

    fn inbound(
        channel: &str,
        sender: &str,
        chat_id: Option<&str>,
        is_group: bool,
        reply_to: Option<&str>,
    ) -> InboundMessage {
        InboundMessage {
            channel: channel.to_string(),
            sender: sender.to_string(),
            content: "hello".to_string(),
            chat_id: chat_id.map(ToOwned::to_owned),
            is_group,
            timestamp: Utc::now(),
            reply_to: reply_to.map(ToOwned::to_owned),
            kind: coop_core::InboundKind::Text,
            message_timestamp: None,
        }
    }

    #[test]
    fn terminal_routes_to_main() {
        let msg = inbound("terminal:default", "alice", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(decision.session_key.agent_id, "reid");
        assert_eq!(decision.session_key.kind, SessionKind::Main);
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn signal_dm_routes_per_sender() {
        let msg = inbound("signal", "alice-uuid", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:alice-uuid".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn unknown_signal_user_is_public() {
        let msg = inbound("signal", "mallory-uuid", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:mallory-uuid".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Public);
    }

    #[test]
    fn signal_group_routes_to_group_session_with_familiar_ceiling() {
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Group("signal:group:deadbeef".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Familiar);
    }

    #[test]
    fn terminal_reply_to_dm_routes_to_requested_session() {
        let msg = inbound(
            "terminal:default",
            "alice",
            None,
            false,
            Some("reid:dm:signal:bob-uuid"),
        );
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:bob-uuid".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn terminal_reply_to_group_applies_familiar_ceiling() {
        let msg = inbound(
            "terminal:default",
            "alice",
            None,
            false,
            Some("reid:group:signal:group:deadbeef"),
        );
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Group("signal:group:deadbeef".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Familiar);
    }

    #[test]
    fn terminal_ignores_invalid_reply_to() {
        let msg = inbound(
            "terminal:default",
            "alice",
            None,
            false,
            Some("other:dm:signal:bob-uuid"),
        );
        let decision = route_message(&msg, &test_config());

        assert_eq!(decision.session_key.kind, SessionKind::Main);
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    fn test_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "You are a test agent.").unwrap();
        dir
    }

    #[tokio::test]
    async fn dispatch_routes_and_runs_turn() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello from fake"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                config.clone(),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(config, gateway.clone());

        let msg = inbound("signal", "bob-uuid", None, false, None);
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let decision = router.dispatch(&msg, event_tx).await.unwrap();
        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:bob-uuid".to_string())
        );

        let mut saw_done = false;
        while let Some(event) = event_rx.recv().await {
            if matches!(event, TurnEvent::Done(_)) {
                saw_done = true;
                break;
            }
        }

        assert!(saw_done);
        assert!(
            gateway
                .list_sessions()
                .iter()
                .any(|key| key == &decision.session_key)
        );
    }

    #[tokio::test]
    async fn dispatch_collect_text_returns_assistant_reply() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hi from fake"));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                config.clone(),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(config, gateway);

        let msg = inbound("signal", "alice-uuid", None, false, None);
        let (decision, response) = router.dispatch_collect_text(&msg).await.unwrap();

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:alice-uuid".to_string())
        );
        assert_eq!(response, "hi from fake");
    }
}
