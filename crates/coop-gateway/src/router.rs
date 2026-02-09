use anyhow::Result;
use coop_core::{
    InboundKind, InboundMessage, SessionKey, SessionKind, TrustLevel, TurnEvent, TurnResult, Usage,
};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span};

use std::sync::Arc;

use crate::config::{Config, SharedConfig};
use crate::gateway::Gateway;
use crate::trust::resolve_trust;

#[derive(Debug, Clone)]
pub(crate) struct RouteDecision {
    pub session_key: SessionKey,
    pub trust: TrustLevel,
    pub user_name: Option<String>,
}

#[derive(Clone)]
pub(crate) struct MessageRouter {
    config: SharedConfig,
    gateway: Arc<Gateway>,
}

impl MessageRouter {
    pub(crate) fn new(config: SharedConfig, gateway: Arc<Gateway>) -> Self {
        Self { config, gateway }
    }

    pub(crate) fn route(&self, msg: &InboundMessage) -> RouteDecision {
        route_message(msg, &self.config.load())
    }

    #[allow(dead_code)]
    pub(crate) fn session_is_empty(&self, session_key: &SessionKey) -> bool {
        self.gateway.session_is_empty(session_key)
    }

    #[allow(dead_code)]
    pub(crate) fn seed_signal_history(&self, session_key: &SessionKey, history: &[InboundMessage]) {
        self.gateway.seed_signal_history(session_key, history);
    }

    pub(crate) async fn dispatch(
        &self,
        msg: &InboundMessage,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<RouteDecision> {
        let decision = self.route(msg);

        // TODO: Replace this simple full-trust gate with a proper authorization
        // system that supports per-session, per-channel, and per-command policies.
        if !is_trust_authorized(&decision, msg) {
            info!(
                session = %decision.session_key,
                trust = ?decision.trust,
                sender = %msg.sender,
                channel = %msg.channel,
                "message rejected: sender lacks full trust"
            );
            let _ = event_tx.send(TurnEvent::TextDelta(String::new())).await;
            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: Vec::new(),
                    usage: Usage::default(),
                    hit_limit: false,
                }))
                .await;
            return Ok(decision);
        }

        // Intercept slash commands before sending to the LLM.
        // Channels tag these as InboundKind::Command with the raw command
        // text (no envelope prefix), so matching is exact.
        if msg.kind == InboundKind::Command {
            let cmd = msg.content.trim();
            let response = self
                .handle_channel_command(cmd, &decision)
                .unwrap_or_else(|| format!("Unknown command: {cmd}\nType /help for a list."));
            info!(
                session = %decision.session_key,
                command = cmd,
                "channel slash command handled"
            );
            let _ = event_tx.send(TurnEvent::TextDelta(response)).await;
            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: Vec::new(),
                    usage: Usage::default(),
                    hit_limit: false,
                }))
                .await;
            return Ok(decision);
        }

        let span = info_span!(
            "route_message",
            session = %decision.session_key,
            trust = ?decision.trust,
            user = ?decision.user_name,
            source = %msg.channel,
        );
        debug!(parent: &span, sender = %msg.sender, "routing message");
        self.gateway
            .run_turn_with_trust(
                &decision.session_key,
                &msg.content,
                decision.trust,
                decision.user_name.as_deref(),
                Some(&msg.channel),
                event_tx,
            )
            .instrument(span)
            .await?;
        Ok(decision)
    }

    /// Handle slash commands from non-TUI channels (Signal, IPC, etc.).
    /// Returns `Some(response_text)` if the input was a command, `None` otherwise.
    fn handle_channel_command(&self, input: &str, decision: &RouteDecision) -> Option<String> {
        match input {
            "/new" | "/clear" | "/reset" => {
                self.gateway.clear_session(&decision.session_key);
                Some("Session cleared.".to_owned())
            }
            "/status" => {
                let count = self.gateway.session_message_count(&decision.session_key);
                let usage = self.gateway.session_usage(&decision.session_key);
                let context_limit = self.gateway.context_limit();
                #[allow(clippy::cast_precision_loss)]
                let context_pct = if context_limit > 0 {
                    f64::from(usage.last_input_tokens) / (context_limit as f64) * 100.0
                } else {
                    0.0
                };
                let status = format!(
                    "Session: {}\nAgent: {}\nModel: {}\nMessages: {}\nContext: {} / {} tokens ({:.1}%)\nTotal tokens used: {} in / {} out",
                    decision.session_key,
                    self.gateway.agent_id(),
                    self.gateway.model_name(),
                    count,
                    usage.last_input_tokens,
                    context_limit,
                    context_pct,
                    usage.cumulative.input_tokens.unwrap_or(0),
                    usage.cumulative.output_tokens.unwrap_or(0),
                );
                Some(status)
            }
            "/help" | "/?" => Some(
                "Available commands:\n\
                     /new, /clear  — Start a new session (clears history)\n\
                     /status       — Show session info\n\
                     /help, /?     — Show this help"
                    .to_owned(),
            ),
            _ => None,
        }
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
                    text = message;
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

    if msg.channel == "cron" {
        let rest = msg.sender.strip_prefix("cron:").unwrap_or(&msg.sender);
        let (cron_name, cron_user) = match rest.find(':') {
            Some(idx) => (&rest[..idx], Some(rest[idx + 1..].to_owned())),
            None => (rest, None),
        };

        let (user_trust, user_name) = if let Some(ref user) = cron_user {
            let matched = config.users.iter().find(|u| u.name == *user);
            let trust = matched.map_or(TrustLevel::Full, |u| u.trust);
            (trust, Some(user.clone()))
        } else {
            (TrustLevel::Full, None)
        };

        let trust = resolve_trust(user_trust, TrustLevel::Full);

        return RouteDecision {
            session_key: SessionKey {
                agent_id,
                kind: SessionKind::Cron(cron_name.to_owned()),
            },
            trust,
            user_name,
        };
    }

    let identity = format!("{}:{}", msg.channel, msg.sender);

    let explicit_kind = if msg.channel == "terminal:default" {
        msg.reply_to
            .as_deref()
            .and_then(|session| parse_explicit_session_kind(session, &agent_id))
    } else {
        None
    };

    let matched_user = config.users.iter().find(|user| {
        user.r#match.iter().any(|pattern| {
            pattern == &identity || pattern == &msg.channel || pattern == &msg.sender
        })
    });

    let user_trust = matched_user.map_or(TrustLevel::Public, |user| user.trust);
    let user_name = matched_user.map(|user| user.name.clone());

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
        user_name,
    }
}

/// Simple trust gate: only full-trust users may trigger agent turns.
/// Terminal sessions are always allowed (local physical access).
fn is_trust_authorized(decision: &RouteDecision, msg: &InboundMessage) -> bool {
    if msg.channel.starts_with("terminal") {
        return true;
    }
    decision.trust == TrustLevel::Full
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
        return Some(SessionKind::Dm(dm.to_owned()));
    }

    if let Some(group) = rest.strip_prefix("group:") {
        return Some(SessionKind::Group(group.to_owned()));
    }

    None
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::shared_config;
    use chrono::Utc;
    use coop_core::Provider;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn test_config() -> Config {
        serde_yaml::from_str(
            "
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
            channel: channel.to_owned(),
            sender: sender.to_owned(),
            content: "hello".to_owned(),
            chat_id: chat_id.map(ToOwned::to_owned),
            is_group,
            timestamp: Utc::now(),
            reply_to: reply_to.map(ToOwned::to_owned),
            kind: InboundKind::Text,
            message_timestamp: None,
        }
    }

    #[test]
    fn trust_gate_allows_full_trust() {
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Dm("signal:alice-uuid".into()),
            },
            trust: TrustLevel::Full,
            user_name: Some("alice".into()),
        };
        let msg = inbound("signal", "alice-uuid", None, false, None);
        assert!(is_trust_authorized(&decision, &msg));
    }

    #[test]
    fn trust_gate_rejects_inner_trust() {
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Dm("signal:bob-uuid".into()),
            },
            trust: TrustLevel::Inner,
            user_name: Some("bob".into()),
        };
        let msg = inbound("signal", "bob-uuid", None, false, None);
        assert!(!is_trust_authorized(&decision, &msg));
    }

    #[test]
    fn trust_gate_rejects_public_trust() {
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Dm("signal:mallory-uuid".into()),
            },
            trust: TrustLevel::Public,
            user_name: None,
        };
        let msg = inbound("signal", "mallory-uuid", None, false, None);
        assert!(!is_trust_authorized(&decision, &msg));
    }

    #[test]
    fn trust_gate_always_allows_terminal() {
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Main,
            },
            trust: TrustLevel::Full,
            user_name: Some("alice".into()),
        };
        let msg = inbound("terminal:default", "alice", None, false, None);
        assert!(is_trust_authorized(&decision, &msg));
    }

    #[test]
    fn terminal_routes_to_main() {
        let msg = inbound("terminal:default", "alice", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(decision.session_key.agent_id, "reid");
        assert_eq!(decision.session_key.kind, SessionKind::Main);
        assert_eq!(decision.trust, TrustLevel::Full);
        assert_eq!(decision.user_name.as_deref(), Some("alice"));
    }

    #[test]
    fn signal_dm_routes_per_sender() {
        let msg = inbound("signal", "alice-uuid", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:alice-uuid".to_owned())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
        assert_eq!(decision.user_name.as_deref(), Some("alice"));
    }

    #[test]
    fn unknown_signal_user_is_public() {
        let msg = inbound("signal", "mallory-uuid", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:mallory-uuid".to_owned())
        );
        assert_eq!(decision.trust, TrustLevel::Public);
        assert_eq!(decision.user_name, None);
    }

    #[test]
    fn signal_group_routes_to_group_session_with_familiar_ceiling() {
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Group("signal:group:deadbeef".to_owned())
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
            SessionKind::Dm("signal:bob-uuid".to_owned())
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
            SessionKind::Group("signal:group:deadbeef".to_owned())
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

    #[test]
    fn cron_with_user_routes_to_cron_session() {
        let msg = inbound("cron", "cron:heartbeat:alice", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Cron("heartbeat".to_owned())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
        assert_eq!(decision.user_name.as_deref(), Some("alice"));
    }

    #[test]
    fn cron_without_user_routes_to_cron_session() {
        let msg = inbound("cron", "cron:heartbeat", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Cron("heartbeat".to_owned())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
        assert_eq!(decision.user_name, None);
    }

    #[test]
    fn cron_with_inner_trust_user() {
        let msg = inbound("cron", "cron:heartbeat:bob", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Cron("heartbeat".to_owned())
        );
        assert_eq!(decision.trust, TrustLevel::Inner);
        assert_eq!(decision.user_name.as_deref(), Some("bob"));
    }

    #[test]
    fn cron_with_unknown_user_defaults_to_full() {
        let msg = inbound("cron", "cron:heartbeat:unknown", None, false, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Cron("heartbeat".to_owned())
        );
        assert_eq!(decision.trust, TrustLevel::Full);
        assert_eq!(decision.user_name.as_deref(), Some("unknown"));
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
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, Arc::clone(&gateway));

        let msg = inbound("signal", "alice-uuid", None, false, None);
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let decision = router.dispatch(&msg, event_tx).await.unwrap();
        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:alice-uuid".to_owned())
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

    /// Provider that always fails — used to test error handling paths.
    #[derive(Debug)]
    struct FailingProvider {
        model: coop_core::ModelInfo,
        error_msg: String,
    }

    impl FailingProvider {
        fn new(msg: &str) -> Self {
            Self {
                model: coop_core::ModelInfo {
                    name: "fail-model".into(),
                    context_limit: 128_000,
                },
                error_msg: msg.to_owned(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn model_info(&self) -> &coop_core::ModelInfo {
            &self.model
        }
        async fn complete(
            &self,
            _s: &str,
            _m: &[coop_core::Message],
            _t: &[coop_core::ToolDef],
        ) -> Result<(coop_core::Message, Usage)> {
            anyhow::bail!("{}", self.error_msg)
        }
        async fn stream(
            &self,
            _s: &str,
            _m: &[coop_core::Message],
            _t: &[coop_core::ToolDef],
        ) -> Result<coop_core::traits::ProviderStream> {
            anyhow::bail!("{}", self.error_msg)
        }
    }

    #[tokio::test]
    async fn dispatch_collect_text_returns_error_as_text() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new(
            "Anthropic API error: 500 - overloaded",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, gateway);

        // alice has trust: full, so she should see the real error
        let msg = inbound("signal", "alice-uuid", None, false, None);
        let result = router.dispatch_collect_text(&msg).await;

        assert!(result.is_ok(), "should not crash on provider error");
        let (_decision, response) = result.unwrap();
        assert!(
            response.contains("500"),
            "full-trust user should see error detail: {response}"
        );
    }

    #[tokio::test]
    async fn dispatch_rejects_public_user_silently() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> =
            Arc::new(FakeProvider::new("should not reach LLM for public user"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, Arc::clone(&gateway));

        // mallory is unknown → public trust, should be silently rejected
        let msg = inbound("signal", "mallory-uuid", None, false, None);
        let (decision, response) = router.dispatch_collect_text(&msg).await.unwrap();

        assert_eq!(decision.trust, TrustLevel::Public);
        assert!(
            response.is_empty(),
            "public user should get no response: {response}"
        );
        assert_eq!(
            gateway.session_message_count(&decision.session_key),
            0,
            "no session should be created for rejected user"
        );
    }

    #[tokio::test]
    async fn dispatch_rejects_inner_trust_user_on_signal() {
        let config = test_config();
        let (router, gateway) = make_router_and_gateway(&config);

        // bob has inner trust — should be rejected on signal
        let msg = inbound_with_content("signal", "bob-uuid", "hello");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert_eq!(decision.trust, TrustLevel::Inner);
        assert!(text.is_empty(), "inner-trust user should get no response");
        assert_eq!(
            gateway.session_message_count(&decision.session_key),
            0,
            "no session should be created for rejected user"
        );
    }

    #[tokio::test]
    async fn dispatch_allows_full_trust_user_on_signal() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);

        let msg = inbound_with_content("signal", "alice-uuid", "hello");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert_eq!(decision.trust, TrustLevel::Full);
        assert!(
            !text.is_empty(),
            "full-trust user should get a response from the LLM"
        );
    }

    #[tokio::test]
    async fn dispatch_always_allows_terminal_users() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);

        // Terminal users always get through regardless of trust resolution
        let msg = inbound_with_content("terminal:default", "alice", "hello");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert_eq!(decision.trust, TrustLevel::Full);
        assert!(
            !text.is_empty(),
            "terminal user should always get a response"
        );
    }

    #[tokio::test]
    async fn dispatch_collect_text_returns_assistant_reply() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hi from fake"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, gateway);

        let msg = inbound("signal", "alice-uuid", None, false, None);
        let (decision, response) = router.dispatch_collect_text(&msg).await.unwrap();

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Dm("signal:alice-uuid".to_owned())
        );
        assert_eq!(response, "hi from fake");
    }

    fn make_router_and_gateway(config: &Config) -> (MessageRouter, Arc<Gateway>) {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("should not reach LLM"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config.clone());
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, Arc::clone(&gateway));
        (router, gateway)
    }

    fn inbound_with_content(channel: &str, sender: &str, content: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_owned(),
            sender: sender.to_owned(),
            content: content.to_owned(),
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Text,
            message_timestamp: None,
        }
    }

    fn inbound_command(channel: &str, sender: &str, command: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_owned(),
            sender: sender.to_owned(),
            content: command.to_owned(),
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Command,
            message_timestamp: None,
        }
    }

    async fn dispatch_and_collect_text(
        router: &MessageRouter,
        msg: &InboundMessage,
    ) -> (RouteDecision, String) {
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let decision = router.dispatch(msg, event_tx).await.unwrap();
        let mut text = String::new();
        while let Some(event) = event_rx.recv().await {
            match event {
                TurnEvent::TextDelta(delta) => text.push_str(&delta),
                TurnEvent::Done(_) => break,
                _ => {}
            }
        }
        (decision, text)
    }

    #[tokio::test]
    async fn slash_help_returns_command_list() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/help");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(text.contains("/new"));
        assert!(text.contains("/status"));
        assert!(text.contains("/help"));
    }

    #[tokio::test]
    async fn slash_question_mark_is_help_alias() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/?");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(text.contains("/new"));
    }

    #[tokio::test]
    async fn slash_status_shows_session_info() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/status");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(text.contains("Session:"));
        assert!(text.contains("Model:"));
        assert!(text.contains("reid"), "should contain agent id");
        assert!(text.contains("Context:"), "should show context usage");
        assert!(text.contains("128000"), "should show context window size");
        assert!(
            text.contains("Total tokens used:"),
            "should show cumulative usage"
        );
    }

    #[tokio::test]
    async fn slash_new_clears_session() {
        let config = test_config();
        let (router, gateway) = make_router_and_gateway(&config);

        // First send a normal message to populate the session
        let msg = inbound_with_content("signal", "alice-uuid", "hello");
        let (decision, _text) = dispatch_and_collect_text(&router, &msg).await;
        assert!(
            gateway.session_message_count(&decision.session_key) > 0,
            "session should have messages after a turn"
        );

        // Now send /new to clear
        let msg = inbound_command("signal", "alice-uuid", "/new");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;
        assert!(text.contains("Session cleared"));
        assert_eq!(
            gateway.session_message_count(&decision.session_key),
            0,
            "session should be empty after /new"
        );
    }

    #[tokio::test]
    async fn slash_clear_is_alias_for_new() {
        let config = test_config();
        let (router, gateway) = make_router_and_gateway(&config);

        let msg = inbound_with_content("signal", "alice-uuid", "hello");
        let (decision, _) = dispatch_and_collect_text(&router, &msg).await;
        assert!(gateway.session_message_count(&decision.session_key) > 0);

        let msg = inbound_command("signal", "alice-uuid", "/clear");
        dispatch_and_collect_text(&router, &msg).await;
        assert_eq!(gateway.session_message_count(&decision.session_key), 0);
    }

    #[tokio::test]
    async fn unknown_command_returns_error() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/bogus");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(text.contains("Unknown command"));
        assert!(text.contains("/bogus"));
    }

    #[tokio::test]
    async fn non_command_reaches_llm() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_with_content("signal", "alice-uuid", "hello there");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        // FakeProvider returns "should not reach LLM" — but it *should* reach it here
        assert!(
            text.contains("should not reach LLM"),
            "non-command should be dispatched to provider"
        );
    }

    #[tokio::test]
    async fn command_with_leading_whitespace_is_recognized() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        // Channel is responsible for detecting commands; content may have
        // leading/trailing whitespace but kind must be Command.
        let msg = inbound_command("signal", "alice-uuid", "  /help  ");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(text.contains("/new"), "trimmed input should match /help");
    }
}
