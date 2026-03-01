use anyhow::Result;
use coop_core::{
    InboundKind, InboundMessage, SessionKey, SessionKind, TrustLevel, TurnEvent, TurnResult, Usage,
};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span};

use std::sync::Arc;

use crate::config::{Config, GroupConfig, GroupTrigger, SharedConfig, TrustCeiling};
use crate::gateway::Gateway;
use crate::group_trigger::{self, TriggerDecision};
use crate::injection::SessionInjection;
use crate::trust::resolve_trust;

enum GroupTriggerOutcome {
    /// Trigger fired — respond with this (possibly history-enriched) input.
    Respond(String),
    /// Trigger did not fire — message was buffered, skip the turn.
    Skip,
}

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

    /// Whether the Signal channel is configured with `verbose: true`.
    #[cfg(feature = "signal")]
    pub(crate) fn signal_verbose(&self) -> bool {
        self.config
            .load()
            .channels
            .signal
            .as_ref()
            .is_some_and(|s| s.verbose)
    }

    pub(crate) fn append_to_session(&self, session_key: &SessionKey, message: coop_core::Message) {
        self.gateway.append_message(session_key, message);
    }

    /// Returns `true` if the given session has an in-progress turn.
    pub(crate) fn has_active_turn(&self, session_key: &SessionKey) -> bool {
        self.gateway.has_active_turn(session_key)
    }

    /// Returns the per-session turn lock. Acquire before appending messages to
    /// a session that may have an in-progress turn (e.g. cron injections).
    pub(crate) fn session_turn_lock(
        &self,
        session_key: &SessionKey,
    ) -> Arc<tokio::sync::Mutex<()>> {
        self.gateway.session_turn_lock(session_key)
    }

    pub(crate) async fn dispatch(
        &self,
        msg: &InboundMessage,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<RouteDecision> {
        self.dispatch_inner(msg, event_tx, None).await
    }

    #[allow(dead_code)]
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
            user = ?decision.user_name,
            source = ?injection.source,
        );
        debug!(
            parent: &span,
            prompt_channel = ?injection.prompt_channel,
            content_len = injection.content.len(),
            "routing session injection"
        );

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

    /// Dispatch with an explicit channel override for prompt context.
    ///
    /// When `prompt_channel` is `Some`, the prompt builder uses it for
    /// channel-specific formatting instructions instead of `msg.channel`.
    /// This lets cron jobs format for the delivery channel (e.g. Signal)
    /// while still routing through the cron session.
    async fn dispatch_inner(
        &self,
        msg: &InboundMessage,
        event_tx: mpsc::Sender<TurnEvent>,
        prompt_channel: Option<&str>,
    ) -> Result<RouteDecision> {
        let decision = self.route(msg);

        // Intercept slash commands before other authorization.
        // Slash commands require the same trust authorization as regular messages.
        if msg.kind == InboundKind::Command {
            let cfg = self.config.load();
            if !is_trust_authorized(&decision, msg, &cfg) {
                info!(
                    session = %decision.session_key,
                    trust = ?decision.trust,
                    sender = %msg.sender,
                    channel = %msg.channel,
                    command = %msg.content.trim(),
                    "slash command rejected: sender lacks full trust"
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

        let cfg = self.config.load();
        if !is_trust_authorized(&decision, msg, &cfg) {
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

        // Evaluate group trigger — decide whether to respond or buffer.
        let user_input = if msg.is_group {
            let cfg = self.config.load();
            match self
                .evaluate_group_trigger(msg, &decision, &cfg, &event_tx)
                .await
            {
                GroupTriggerOutcome::Respond(input) => input,
                GroupTriggerOutcome::Skip => return Ok(decision),
            }
        } else {
            msg.content.clone()
        };

        let span = info_span!(
            "route_message",
            session = %decision.session_key,
            trust = ?decision.trust,
            user = ?decision.user_name,
            source = %msg.channel,
        );
        debug!(parent: &span, sender = %msg.sender, "routing message");
        let channel = prompt_channel.unwrap_or(&msg.channel);
        self.gateway
            .run_turn_with_trust(
                &decision.session_key,
                &user_input,
                decision.trust,
                decision.user_name.as_deref(),
                Some(channel),
                event_tx,
            )
            .instrument(span)
            .await?;
        Ok(decision)
    }

    /// Evaluate group trigger and decide whether to respond or buffer.
    async fn evaluate_group_trigger(
        &self,
        msg: &InboundMessage,
        decision: &RouteDecision,
        config: &Config,
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> GroupTriggerOutcome {
        let Some(group_config) = find_group_config(msg, config) else {
            return GroupTriggerOutcome::Respond(msg.content.clone());
        };

        // If the message has unresolved mentions (@mention sentinel from
        // inbound parsing), always respond — we can't confirm it *wasn't*
        // directed at this agent, and ignoring a mention is worse than
        // responding to one not meant for us.
        let has_unresolved_mention = has_unresolved_mention_sentinel(&msg.content);
        if has_unresolved_mention {
            debug!(
                session = %decision.session_key,
                "auto-triggering: message contains unresolved @mention"
            );
        }

        let should_respond = has_unresolved_mention
            || match group_config.trigger {
                GroupTrigger::Always => true,
                GroupTrigger::Llm => {
                    let history_context = self.gateway.peek_group_history(&decision.session_key);
                    let full_input =
                        prepend_history_context(&msg.content, history_context.as_deref());
                    self.gateway
                        .evaluate_llm_trigger(
                            &decision.session_key,
                            &full_input,
                            decision.trust,
                            decision.user_name.as_deref(),
                            Some(&msg.channel),
                            group_config,
                        )
                        .await
                }
                GroupTrigger::Mention | GroupTrigger::Regex => {
                    group_trigger::evaluate_trigger(msg, group_config, &config.agent.id)
                        == TriggerDecision::Respond
                }
            };

        if !should_respond {
            self.gateway.record_group_history(
                &decision.session_key,
                msg,
                group_config.history_limit,
            );
            debug!(
                session = %decision.session_key,
                trigger = %group_config.trigger,
                "group message skipped by trigger"
            );
            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: Vec::new(),
                    usage: Usage::default(),
                    hit_limit: false,
                }))
                .await;
            return GroupTriggerOutcome::Skip;
        }

        // Drain buffered history and prepend to the message content.
        let history_context = self.gateway.drain_group_history(&decision.session_key);
        GroupTriggerOutcome::Respond(prepend_history_context(
            &msg.content,
            history_context.as_deref(),
        ))
    }

    /// Handle slash commands from non-TUI channels (Signal, IPC, etc.).
    /// Returns `Some(response_text)` if the input was a command, `None` otherwise.
    fn handle_channel_command(&self, input: &str, decision: &RouteDecision) -> Option<String> {
        match input {
            "/new" | "/clear" | "/reset" => {
                self.gateway.clear_session(&decision.session_key);
                Some("New session ✅".to_owned())
            }
            "/stop" => {
                if self.gateway.cancel_active_turn(&decision.session_key) {
                    Some("Stopping agent…".to_owned())
                } else {
                    Some("No active turn to stop.".to_owned())
                }
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
                let active = if self.gateway.has_active_turn(&decision.session_key) {
                    " (running)"
                } else {
                    ""
                };
                let status = format!(
                    "Session: {}{active}\nAgent: {}\nModel: {}\nMessages: {}\nContext: {} / {} tokens ({:.1}%)\nTotal tokens used: {} in / {} out",
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
                     /stop         — Stop the current agent turn\n\
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
        self.dispatch_collect_text_with_channel(msg, None).await
    }

    /// Like `dispatch_collect_text` but with an explicit channel override for
    /// prompt context (used by cron to format responses for delivery channels).
    pub(crate) async fn dispatch_collect_text_with_channel(
        &self,
        msg: &InboundMessage,
        prompt_channel: Option<String>,
    ) -> Result<(RouteDecision, String)> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let router = self.clone();
        let message = msg.clone();

        let dispatch_task = tokio::spawn(async move {
            router
                .dispatch_inner(&message, event_tx, prompt_channel.as_deref())
                .await
        });

        let mut text = String::new();

        while let Some(event) = event_rx.recv().await {
            match event {
                TurnEvent::TextDelta(delta) => {
                    text.push_str(&delta);
                }
                TurnEvent::AssistantMessage(ref message) => {
                    // Only keep the final assistant response (the one without
                    // tool requests). Intermediate "thinking" text before tool
                    // calls is not useful for delivery.
                    let msg_text = message.text();
                    if !message.has_tool_requests() && !msg_text.is_empty() {
                        text = msg_text;
                    }
                }
                TurnEvent::Error(message) => {
                    text = message;
                }
                TurnEvent::Done(_) => {
                    break;
                }
                TurnEvent::ToolStart { .. }
                | TurnEvent::ToolResult { .. }
                | TurnEvent::Compacting => {}
            }
        }

        let decision = match dispatch_task.await {
            Ok(result) => result?,
            Err(error) => anyhow::bail!("router task failed: {error}"),
        };

        Ok((decision, text))
    }

    #[allow(dead_code)]
    pub(crate) async fn inject_collect_text(
        &self,
        injection: &SessionInjection,
    ) -> Result<(RouteDecision, String)> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let router = self.clone();
        let injection = injection.clone();

        let dispatch_task =
            tokio::spawn(async move { router.dispatch_injection(&injection, event_tx).await });

        let mut text = String::new();
        let mut turn_error: Option<String> = None;

        while let Some(event) = event_rx.recv().await {
            match event {
                TurnEvent::TextDelta(delta) => {
                    text.push_str(&delta);
                }
                TurnEvent::AssistantMessage(ref message) => {
                    let msg_text = message.text();
                    if !message.has_tool_requests() && !msg_text.is_empty() {
                        text = msg_text;
                    }
                }
                TurnEvent::Error(message) => {
                    turn_error = Some(message);
                }
                TurnEvent::Done(_) => {
                    break;
                }
                TurnEvent::ToolStart { .. }
                | TurnEvent::ToolResult { .. }
                | TurnEvent::Compacting => {}
            }
        }

        let decision = match dispatch_task.await {
            Ok(result) => result?,
            Err(error) => anyhow::bail!("router task failed: {error}"),
        };

        if let Some(message) = turn_error {
            anyhow::bail!(message);
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

        let trust = resolve_trust(user_trust, TrustLevel::Owner);

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

    let group_config = find_group_config(msg, config);

    let user_trust = matched_user.map_or_else(
        || {
            if msg.channel == "terminal:default" && config.sandbox.enabled {
                TrustLevel::Owner
            } else if let Some(gc) = group_config {
                gc.default_trust
            } else {
                TrustLevel::Public
            }
        },
        |user| user.trust,
    );
    let user_name = matched_user.map(|user| user.name.clone());

    let group_context = msg.is_group
        || explicit_kind
            .as_ref()
            .is_some_and(|kind| matches!(kind, SessionKind::Group(_)));

    let ceiling = if group_context {
        match group_config.map(|gc| &gc.trust_ceiling) {
            Some(TrustCeiling::Fixed(level)) => *level,
            // min_member requires async query; None/wildcard = no ceiling.
            // Resolved later in dispatch for min_member.
            _ => TrustLevel::Owner,
        }
    } else {
        TrustLevel::Owner
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

/// Trust gate: only owner/full-trust users may trigger agent turns.
/// Terminal sessions are always allowed (local physical access).
/// Configured groups are explicitly opted-in via `[[groups]]`.
fn is_trust_authorized(decision: &RouteDecision, msg: &InboundMessage, config: &Config) -> bool {
    if msg.channel.starts_with("terminal") {
        return true;
    }
    match &decision.session_key.kind {
        SessionKind::Group(_) => find_group_config(msg, config).is_some(),
        _ => decision.trust <= TrustLevel::Full,
    }
}

pub(crate) fn find_group_config<'a>(
    msg: &InboundMessage,
    config: &'a Config,
) -> Option<&'a GroupConfig> {
    if !msg.is_group {
        return None;
    }
    let chat_id = msg.chat_id.as_deref()?;
    let namespaced = if chat_id.starts_with(&format!("{}:", msg.channel)) {
        chat_id.to_owned()
    } else {
        format!("{}:{chat_id}", msg.channel)
    };
    config.groups.iter().find(|g| {
        g.r#match
            .iter()
            .any(|pattern| pattern == &namespaced || pattern == "*")
    })
}

/// The `@mention` sentinel injected by inbound parsing when a U+FFFC
/// placeholder could not be resolved to a real user name.
const UNRESOLVED_MENTION_SENTINEL: &str = "@mention";

/// Check if the message body (after stripping envelope prefix) contains the
/// unresolved-mention sentinel, indicating someone was @-tagged but we
/// couldn't determine who.
fn has_unresolved_mention_sentinel(content: &str) -> bool {
    let body = group_trigger::strip_envelope_prefix(content);
    body.contains(UNRESOLVED_MENTION_SENTINEL)
}

fn prepend_history_context(message: &str, history: Option<&str>) -> String {
    match history {
        Some(ctx) => format!("{ctx}\n{message}"),
        None => message.to_owned(),
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
    use crate::config::{
        GroupConfig, GroupTrigger, TrustCeiling, find_group_config_by_session, shared_config,
    };
    use crate::injection::{InjectionSource, SessionInjection};
    use crate::provider_registry::ProviderRegistry;
    use chrono::Utc;
    use coop_core::Provider;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn test_config() -> Config {
        toml::from_str(
            r#"
[agent]
id = "reid"
model = "test"

[[users]]
name = "alice"
trust = "full"
match = ["terminal:default", "signal:alice-uuid"]

[[users]]
name = "bob"
trust = "inner"
match = ["signal:bob-uuid"]
"#,
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
            group_revision: None,
        }
    }

    #[test]
    fn trust_gate_allows_full_trust() {
        let cfg = test_config();
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Dm("signal:alice-uuid".into()),
            },
            trust: TrustLevel::Full,
            user_name: Some("alice".into()),
        };
        let msg = inbound("signal", "alice-uuid", None, false, None);
        assert!(is_trust_authorized(&decision, &msg, &cfg));
    }

    #[test]
    fn trust_gate_rejects_inner_trust() {
        let cfg = test_config();
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Dm("signal:bob-uuid".into()),
            },
            trust: TrustLevel::Inner,
            user_name: Some("bob".into()),
        };
        let msg = inbound("signal", "bob-uuid", None, false, None);
        assert!(!is_trust_authorized(&decision, &msg, &cfg));
    }

    #[test]
    fn trust_gate_rejects_public_trust() {
        let cfg = test_config();
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Dm("signal:mallory-uuid".into()),
            },
            trust: TrustLevel::Public,
            user_name: None,
        };
        let msg = inbound("signal", "mallory-uuid", None, false, None);
        assert!(!is_trust_authorized(&decision, &msg, &cfg));
    }

    #[test]
    fn trust_gate_always_allows_terminal() {
        let cfg = test_config();
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Main,
            },
            trust: TrustLevel::Full,
            user_name: Some("alice".into()),
        };
        let msg = inbound("terminal:default", "alice", None, false, None);
        assert!(is_trust_authorized(&decision, &msg, &cfg));
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
    fn signal_group_no_config_routes_with_public_trust() {
        // No [[groups]] in test_config → unknown group → Public trust
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true, None);
        let decision = route_message(&msg, &test_config());

        assert_eq!(
            decision.session_key.kind,
            SessionKind::Group("signal:group:deadbeef".to_owned())
        );
        // alice-uuid matches [[users]] so gets Full, but group has no config
        // so unknown group members would get Public. Alice is matched though.
        assert_eq!(decision.trust, TrustLevel::Full);
    }

    #[test]
    fn signal_group_with_config_uses_default_trust_for_unknown() {
        let mut config = test_config();
        config.groups.push(GroupConfig {
            r#match: vec!["signal:group:deadbeef".to_owned()],
            trigger: GroupTrigger::Mention,
            mention_names: vec!["coop".to_owned()],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        });
        // mallory-uuid is not in [[users]] → gets group's default_trust
        let msg = inbound("signal", "mallory-uuid", Some("group:deadbeef"), true, None);
        let decision = route_message(&msg, &config);
        assert_eq!(decision.trust, TrustLevel::Familiar);
    }

    #[test]
    fn signal_group_with_fixed_ceiling_caps_known_user() {
        let mut config = test_config();
        config.groups.push(GroupConfig {
            r#match: vec!["signal:group:deadbeef".to_owned()],
            trigger: GroupTrigger::Always,
            mention_names: vec![],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::Fixed(TrustLevel::Familiar),
            history_limit: 50,
        });
        // alice has Full trust but ceiling is Familiar
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true, None);
        let decision = route_message(&msg, &config);
        assert_eq!(decision.trust, TrustLevel::Familiar);
    }

    #[test]
    fn group_auth_requires_config() {
        let cfg = test_config(); // no [[groups]]
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Group("signal:group:deadbeef".into()),
            },
            trust: TrustLevel::Familiar,
            user_name: None,
        };
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true, None);
        assert!(!is_trust_authorized(&decision, &msg, &cfg));
    }

    #[test]
    fn group_auth_passes_with_config() {
        let mut cfg = test_config();
        cfg.groups.push(GroupConfig {
            r#match: vec!["signal:group:deadbeef".to_owned()],
            trigger: GroupTrigger::Always,
            mention_names: vec![],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        });
        let decision = RouteDecision {
            session_key: SessionKey {
                agent_id: "reid".into(),
                kind: SessionKind::Group("signal:group:deadbeef".into()),
            },
            trust: TrustLevel::Familiar,
            user_name: None,
        };
        let msg = inbound("signal", "alice-uuid", Some("group:deadbeef"), true, None);
        assert!(is_trust_authorized(&decision, &msg, &cfg));
    }

    #[test]
    fn find_group_config_matches_exact() {
        let mut cfg = test_config();
        cfg.groups.push(GroupConfig {
            r#match: vec!["signal:group:aabb".to_owned()],
            trigger: GroupTrigger::Mention,
            mention_names: vec!["coop".to_owned()],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        });
        let msg = inbound("signal", "alice-uuid", Some("group:aabb"), true, None);
        assert!(find_group_config(&msg, &cfg).is_some());
    }

    #[test]
    fn find_group_config_matches_wildcard() {
        let mut cfg = test_config();
        cfg.groups.push(GroupConfig {
            r#match: vec!["*".to_owned()],
            trigger: GroupTrigger::Always,
            mention_names: vec![],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        });
        let msg = inbound("signal", "alice-uuid", Some("group:anything"), true, None);
        assert!(find_group_config(&msg, &cfg).is_some());
    }

    #[test]
    fn find_group_config_returns_none_for_unconfigured() {
        let cfg = test_config();
        let msg = inbound("signal", "alice-uuid", Some("group:unknown"), true, None);
        assert!(find_group_config(&msg, &cfg).is_none());
    }

    #[test]
    fn find_group_config_by_session_matches() {
        let mut cfg = test_config();
        cfg.groups.push(GroupConfig {
            r#match: vec!["signal:group:aabb".to_owned()],
            trigger: GroupTrigger::Mention,
            mention_names: vec!["coop".to_owned()],
            trigger_regex: None,
            trigger_model: None,
            trigger_prompt: None,
            default_trust: TrustLevel::Familiar,
            trust_ceiling: TrustCeiling::None,
            history_limit: 50,
        });
        let key = SessionKey {
            agent_id: "reid".into(),
            kind: SessionKind::Group("signal:group:aabb".into()),
        };
        assert!(find_group_config_by_session(&key, &cfg).is_some());
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
    fn terminal_reply_to_group_keeps_user_trust_no_config() {
        // No [[groups]] config → no ceiling applied → user keeps their trust
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
        assert_eq!(decision.trust, TrustLevel::Full);
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
                ProviderRegistry::new(provider),
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
        fn model_info(&self) -> coop_core::ModelInfo {
            self.model.clone()
        }
        async fn complete(
            &self,
            _s: &[String],
            _m: &[coop_core::Message],
            _t: &[coop_core::ToolDef],
        ) -> Result<(coop_core::Message, Usage)> {
            anyhow::bail!("{}", self.error_msg)
        }
        async fn stream(
            &self,
            _s: &[String],
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
                ProviderRegistry::new(provider),
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
                ProviderRegistry::new(provider),
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
                ProviderRegistry::new(provider),
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

    #[tokio::test]
    async fn inject_collect_text_runs_turn_on_target_session() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("injection reply"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                ProviderRegistry::new(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, Arc::clone(&gateway));

        let target = SessionKey {
            agent_id: "reid".to_owned(),
            kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
        };

        let injection = SessionInjection {
            target: target.clone(),
            content: "summarize this".to_owned(),
            trust: TrustLevel::Full,
            user_name: Some("alice".to_owned()),
            prompt_channel: Some("signal".to_owned()),
            source: InjectionSource::Cron("heartbeat".to_owned()),
        };

        let (decision, response) = router.inject_collect_text(&injection).await.unwrap();

        assert_eq!(decision.session_key, target);
        assert_eq!(decision.trust, TrustLevel::Full);
        assert_eq!(response, "injection reply");
        assert_eq!(gateway.session_message_count(&decision.session_key), 2);
    }

    #[tokio::test]
    async fn inject_collect_text_uses_explicit_trust() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("public injection reply"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                ProviderRegistry::new(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, Arc::clone(&gateway));

        let injection = SessionInjection {
            target: SessionKey {
                agent_id: "reid".to_owned(),
                kind: SessionKind::Dm("signal:mallory-uuid".to_owned()),
            },
            content: "announce something".to_owned(),
            trust: TrustLevel::Public,
            user_name: None,
            prompt_channel: Some("signal".to_owned()),
            source: InjectionSource::System,
        };

        let (decision, response) = router.inject_collect_text(&injection).await.unwrap();

        assert_eq!(decision.trust, TrustLevel::Public);
        assert_eq!(response, "public injection reply");
        assert_eq!(gateway.session_message_count(&decision.session_key), 2);
    }

    #[tokio::test]
    async fn inject_collect_text_returns_err_on_turn_error() {
        let workspace = test_workspace();
        let config = test_config();
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new("injection failed"));
        let executor = Arc::new(DefaultExecutor::new());
        let shared = shared_config(config);
        let gateway = Arc::new(
            Gateway::new(
                Arc::clone(&shared),
                workspace.path().to_path_buf(),
                ProviderRegistry::new(provider),
                executor,
                None,
                None,
            )
            .unwrap(),
        );
        let router = MessageRouter::new(shared, gateway);

        let injection = SessionInjection {
            target: SessionKey {
                agent_id: "reid".to_owned(),
                kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
            },
            content: "announce".to_owned(),
            trust: TrustLevel::Full,
            user_name: Some("alice".to_owned()),
            prompt_channel: Some("signal".to_owned()),
            source: InjectionSource::Cron("heartbeat".to_owned()),
        };

        let result = router.inject_collect_text(&injection).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("injection failed"), "unexpected error: {err}");
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
                ProviderRegistry::new(provider),
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
            group_revision: None,
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
            group_revision: None,
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
        assert!(text.contains("New session"));
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

    #[tokio::test]
    async fn slash_stop_with_no_active_turn() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/stop");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(
            text.contains("No active turn"),
            "should indicate no turn is running: {text}"
        );
    }

    #[tokio::test]
    async fn slash_help_includes_stop() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/help");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(text.contains("/stop"), "help should list /stop command");
    }

    #[tokio::test]
    async fn slash_status_shows_running_state() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);
        let msg = inbound_command("signal", "alice-uuid", "/status");
        let (_decision, text) = dispatch_and_collect_text(&router, &msg).await;

        // With no active turn, status should not show "(running)"
        assert!(
            !text.contains("(running)"),
            "should not show running when idle: {text}"
        );
    }

    #[tokio::test]
    async fn slash_commands_require_full_trust() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);

        // Alice has full trust - should be able to use slash commands
        let msg = inbound_command("signal", "alice-uuid", "/status");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;
        assert_eq!(decision.trust, TrustLevel::Full);
        assert!(
            text.contains("Session:"),
            "full trust user should get command response"
        );
    }

    #[tokio::test]
    async fn slash_commands_reject_inner_trust() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);

        // Bob has inner trust - should be rejected for slash commands
        let msg = inbound_command("signal", "bob-uuid", "/status");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;
        assert_eq!(decision.trust, TrustLevel::Inner);
        assert!(
            text.is_empty(),
            "inner trust user should not get command response"
        );
    }

    #[tokio::test]
    async fn slash_commands_reject_public_trust() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);

        // Unknown user has public trust - should be rejected for slash commands
        let msg = inbound_command("signal", "mallory-uuid", "/help");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;
        assert_eq!(decision.trust, TrustLevel::Public);
        assert!(
            text.is_empty(),
            "public trust user should not get command response"
        );
    }

    #[tokio::test]
    async fn slash_commands_always_allowed_on_terminal() {
        let config = test_config();
        let (router, _gw) = make_router_and_gateway(&config);

        // Terminal access always allows slash commands regardless of trust
        let msg = inbound_command("terminal:default", "alice", "/status");
        let (decision, text) = dispatch_and_collect_text(&router, &msg).await;
        assert_eq!(decision.trust, TrustLevel::Full);
        assert!(
            text.contains("Session:"),
            "terminal user should always get command response"
        );
    }

    #[tokio::test]
    async fn slash_status_context_reflects_cache_tokens() {
        use crate::gateway::SessionUsage;
        use coop_core::types::Usage;

        let config = test_config();
        let (router, gw) = make_router_and_gateway(&config);

        // First, dispatch a message to establish a session key.
        let msg = inbound_command("signal", "alice-uuid", "/status");
        let (decision, _) = dispatch_and_collect_text(&router, &msg).await;

        // Seed session_usage with values that simulate prompt caching:
        //   input_tokens=300, cache_read=8000, cache_write=1700
        //   → real context = 10000
        gw.set_session_usage(
            &decision.session_key,
            SessionUsage {
                last_input_tokens: 10_000,
                cumulative: Usage {
                    input_tokens: Some(1500),
                    output_tokens: Some(400),
                    ..Default::default()
                },
            },
        );

        // Now call /status and verify the context line uses the full 10000.
        let msg = inbound_command("signal", "alice-uuid", "/status");
        let (_, text) = dispatch_and_collect_text(&router, &msg).await;

        assert!(
            text.contains("Context: 10000 / 128000 tokens"),
            "context should show 10000 (including cache tokens), got: {text}"
        );
        assert!(
            text.contains("Total tokens used: 1500 in / 400 out"),
            "cumulative should show correct totals, got: {text}"
        );
    }
}
