use anyhow::Result;
use coop_core::{InboundMessage, SessionKey, SessionKind, TrustLevel, TurnEvent};
use tokio::sync::mpsc;

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
        self.gateway
            .run_turn_with_trust(
                &decision.session_key,
                &msg.content,
                decision.trust,
                event_tx,
            )
            .await?;
        Ok(decision)
    }
}

pub(crate) fn route_message(msg: &InboundMessage, config: &Config) -> RouteDecision {
    let agent_id = config.agent.id.clone();
    let identity = format!("{}:{}", msg.channel, msg.sender);

    let user_trust = config
        .users
        .iter()
        .find(|user| {
            user.r#match.iter().any(|pattern| {
                pattern == &identity || pattern == &msg.channel || pattern == &msg.sender
            })
        })
        .map_or(TrustLevel::Public, |user| user.trust);

    let ceiling = if msg.is_group {
        TrustLevel::Familiar
    } else {
        TrustLevel::Full
    };
    let trust = resolve_trust(user_trust, ceiling);

    let kind = if msg.is_group {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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
    ) -> InboundMessage {
        InboundMessage {
            channel: channel.to_string(),
            sender: sender.to_string(),
            content: "hello".to_string(),
            chat_id: chat_id.map(ToOwned::to_owned),
            is_group,
            timestamp: Utc::now(),
            reply_to: None,
        }
    }

    #[test]
    fn terminal_routes_to_main() {
        let msg = inbound("terminal:default", "alice", None, false);
        let decision = route_message(&msg, &test_config());

        assert_eq!(decision.session_key.agent_id, "reid");
        assert_eq!(decision.session_key.kind, SessionKind::Main);
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn signal_dm_routes_per_sender() {
        let msg = inbound("signal", "alice-uuid", None, false);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:alice-uuid".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn unknown_signal_user_is_public() {
        let msg = inbound("signal", "mallory-uuid", None, false);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:mallory-uuid".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Public);
    }

    #[test]
    fn signal_group_routes_to_group_session_with_familiar_ceiling() {
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Group("signal:group:deadbeef".to_string())
        );
        assert_eq!(decision.trust, TrustLevel::Familiar);
    }
}
