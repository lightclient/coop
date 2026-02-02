use coop_core::{InboundMessage, SessionKey, SessionKind, TrustLevel};

use crate::config::Config;
use crate::trust::resolve_trust;

/// Result of routing an inbound message.
#[derive(Debug)]
pub(crate) struct RouteDecision {
    pub session_key: SessionKey,
    pub trust: TrustLevel,
}

/// Route an inbound message to the appropriate session.
/// Phase 1: everything goes to the single agent's main session with full trust.
pub(crate) fn route_message(msg: &InboundMessage, config: &Config) -> RouteDecision {
    let agent_id = config.agent.id.clone();

    // Look up user trust
    let user_trust = config
        .users
        .iter()
        .find(|u| {
            u.r#match
                .iter()
                .any(|m| m == &msg.channel || m == &msg.sender)
        })
        .map_or(TrustLevel::Public, |u| u.trust);

    // Determine situation ceiling
    let ceiling = if msg.is_group {
        TrustLevel::Familiar
    } else {
        TrustLevel::Full
    };

    let trust = resolve_trust(user_trust, ceiling);

    let kind = if msg.is_group {
        SessionKind::Group(msg.chat_id.clone().unwrap_or_else(|| msg.channel.clone()))
    } else {
        SessionKind::Main
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
    match: ['terminal:default']
",
        )
        .unwrap()
    }

    #[test]
    fn known_user_dm_routes_to_main() {
        let msg = InboundMessage {
            channel: "terminal:default".to_string(),
            sender: "alice".to_string(),
            content: "hello".to_string(),
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
        };

        let decision = route_message(&msg, &test_config());
        assert_eq!(decision.session_key.agent_id, "reid");
        assert_eq!(decision.session_key.kind, SessionKind::Main);
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn unknown_user_gets_public() {
        let msg = InboundMessage {
            channel: "signal:+15551234567".to_string(),
            sender: "unknown".to_string(),
            content: "hello".to_string(),
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
        };

        let decision = route_message(&msg, &test_config());
        assert_eq!(decision.trust, TrustLevel::Public);
    }
}
