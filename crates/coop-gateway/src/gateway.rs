use anyhow::Result;
use coop_core::prompt::{PromptBuilder, WorkspaceIndex, default_file_configs};
use coop_core::{
    Message, Provider, SessionKey, SessionKind, ToolContext, ToolDef, ToolExecutor, TrustLevel,
    TurnConfig, TurnEvent, TurnResult, TypingNotifier, Usage,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span};
use uuid::Uuid;

use crate::config::Config;

pub(crate) struct Gateway {
    config: Config,
    workspace: PathBuf,
    workspace_index: Mutex<WorkspaceIndex>,
    provider: Arc<dyn Provider>,
    executor: Arc<dyn ToolExecutor>,
    typing_notifier: Option<Arc<dyn TypingNotifier>>,
    sessions: Mutex<HashMap<SessionKey, Vec<Message>>>,
}

struct TypingGuard {
    notifier: Arc<dyn TypingNotifier>,
    session_key: SessionKey,
}

impl TypingGuard {
    fn new(notifier: Arc<dyn TypingNotifier>, session_key: SessionKey) -> Self {
        Self {
            notifier,
            session_key,
        }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        let notifier = Arc::clone(&self.notifier);
        let session_key = self.session_key.clone();
        emit_typing_notifier_event(&session_key, false);
        tokio::spawn(async move {
            notifier.set_typing(&session_key, false).await;
        });
    }
}

fn emit_typing_notifier_event(session_key: &SessionKey, started: bool) {
    let event_name = if started {
        "typing notifier start"
    } else {
        "typing notifier stop"
    };

    if let Some((target_kind, target)) = signal_target_from_session(session_key) {
        info!(
            session = %session_key,
            signal.started = started,
            signal.target_kind = target_kind,
            signal.target = %target,
            "{event_name}"
        );
    } else {
        info!(
            session = %session_key,
            signal.started = started,
            "{event_name}"
        );
    }
}

fn signal_target_from_session(session_key: &SessionKey) -> Option<(&'static str, String)> {
    match &session_key.kind {
        SessionKind::Dm(identity) => {
            let target = identity.strip_prefix("signal:").unwrap_or(identity);
            Some(("direct", target.to_owned()))
        }
        SessionKind::Group(group_id) => {
            let target = group_id.strip_prefix("signal:").unwrap_or(group_id);
            let target = if target.starts_with("group:") {
                target.to_owned()
            } else {
                format!("group:{target}")
            };
            Some(("group", target))
        }
        SessionKind::Main | SessionKind::Isolated(_) => None,
    }
}

impl Gateway {
    pub(crate) fn new(
        config: Config,
        workspace: PathBuf,
        provider: Arc<dyn Provider>,
        executor: Arc<dyn ToolExecutor>,
        typing_notifier: Option<Arc<dyn TypingNotifier>>,
    ) -> Result<Self> {
        let file_configs = default_file_configs();
        let workspace_index = WorkspaceIndex::scan(&workspace, &file_configs)?;

        Ok(Self {
            config,
            workspace,
            workspace_index: Mutex::new(workspace_index),
            provider,
            executor,
            typing_notifier,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Build a trust-gated system prompt for this turn.
    fn build_prompt(&self, trust: TrustLevel) -> Result<String> {
        let file_configs = default_file_configs();
        let mut index = self
            .workspace_index
            .lock()
            .expect("workspace index mutex poisoned");
        let refreshed = index
            .refresh(&self.workspace, &file_configs)
            .unwrap_or(false);
        if refreshed {
            debug!("workspace index refreshed");
        }

        let prompt = PromptBuilder::new(self.workspace.clone(), self.config.agent.id.clone())
            .trust(trust)
            .model(&self.config.agent.model)
            .build(&index)?;
        drop(index);

        Ok(prompt.to_flat_string())
    }

    pub(crate) fn default_session_key(&self) -> SessionKey {
        SessionKey {
            agent_id: self.config.agent.id.clone(),
            kind: SessionKind::Main,
        }
    }

    pub(crate) fn list_sessions(&self) -> Vec<SessionKey> {
        let mut keys: Vec<_> = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .keys()
            .cloned()
            .collect();
        keys.push(self.default_session_key());
        keys.sort_by_cached_key(ToString::to_string);
        keys.dedup_by(|a, b| a.to_string() == b.to_string());
        keys
    }

    pub(crate) fn resolve_session(&self, session: &str) -> Option<SessionKey> {
        parse_session_key(session, &self.config.agent.id)
    }

    fn tool_context(&self, session_key: &SessionKey, trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: session_key.to_string(),
            trust,
            workspace: self.workspace.clone(),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_turn_with_trust(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        let span = info_span!(
            "agent_turn",
            session = %session_key,
            input_len = user_input.len(),
            user_input = user_input,
            trust = ?trust,
        );

        async {
            let _typing_guard = if let Some(notifier) = &self.typing_notifier {
                emit_typing_notifier_event(session_key, true);
                notifier.set_typing(session_key, true).await;
                Some(TypingGuard::new(Arc::clone(notifier), session_key.clone()))
            } else {
                None
            };

            let system_prompt = self.build_prompt(trust)?;
            self.append_message(session_key, Message::user().with_text(user_input));

            let tool_defs = self.executor.tools();
            let ctx = self.tool_context(session_key, trust);
            let turn_config = TurnConfig::default();

            let mut total_usage = Usage::default();
            let mut new_messages = Vec::new();
            let mut hit_limit = false;

            for iteration in 0..turn_config.max_iterations {
                let iter_span = info_span!(
                    "turn_iteration",
                    iteration,
                    max = turn_config.max_iterations,
                );

                let (response, should_break) = async {
                    let messages = self.messages(session_key);
                    let (response, usage) = self
                        .assistant_response(&system_prompt, &messages, &tool_defs, &event_tx)
                        .await?;

                    total_usage += usage;
                    self.append_message(session_key, response.clone());
                    new_messages.push(response.clone());

                    let _ = event_tx
                        .send(TurnEvent::AssistantMessage(response.clone()))
                        .await;

                    info!(
                        has_tool_requests = response.has_tool_requests(),
                        response_text_len = response.text().len(),
                        "iteration complete"
                    );

                    let has_tool_requests = response.has_tool_requests();
                    Ok::<_, anyhow::Error>((response, !has_tool_requests))
                }
                .instrument(iter_span)
                .await?;

                if should_break {
                    break;
                }

                let mut result_msg = Message::user();

                for req in response.tool_requests() {
                    let _ = event_tx
                        .send(TurnEvent::ToolStart {
                            id: req.id.clone(),
                            name: req.name.clone(),
                            arguments: req.arguments.clone(),
                        })
                        .await;

                    let tool_span = info_span!(
                        "tool_execute",
                        tool.name = %req.name,
                        tool.id = %req.id,
                    );

                    let output = async {
                        debug!(arguments = %req.arguments, "tool arguments");
                        match self
                            .executor
                            .execute(&req.name, req.arguments.clone(), &ctx)
                            .await
                        {
                            Ok(output) => {
                                let preview_len = output.content.len().min(500);
                                info!(
                                    output_len = output.content.len(),
                                    is_error = output.is_error,
                                    output_preview = &output.content[..preview_len],
                                    "tool complete"
                                );
                                output
                            }
                            Err(err) => {
                                error!(tool = %req.name, error = %err, "tool execution failed");
                                coop_core::ToolOutput::error(format!("internal error: {err}"))
                            }
                        }
                    }
                    .instrument(tool_span)
                    .await;

                    result_msg =
                        result_msg.with_tool_result(&req.id, &output.content, output.is_error);

                    let _ = event_tx
                        .send(TurnEvent::ToolResult {
                            id: req.id.clone(),
                            message: Message::user().with_tool_result(
                                &req.id,
                                &output.content,
                                output.is_error,
                            ),
                        })
                        .await;
                }

                self.append_message(session_key, result_msg.clone());
                new_messages.push(result_msg);

                if iteration + 1 >= turn_config.max_iterations {
                    hit_limit = true;
                }
            }

            info!(
                input_tokens = total_usage.input_tokens,
                output_tokens = total_usage.output_tokens,
                hit_limit,
                "turn complete"
            );

            let _ = event_tx
                .send(TurnEvent::Done(TurnResult {
                    messages: new_messages,
                    usage: total_usage,
                    hit_limit,
                }))
                .await;

            Ok(())
        }
        .instrument(span)
        .await
    }

    pub(crate) fn clear_session(&self, session_key: &SessionKey) {
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        sessions.remove(session_key);
    }

    fn append_message(&self, session_key: &SessionKey, message: Message) {
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        sessions
            .entry(session_key.clone())
            .or_default()
            .push(message);
    }

    fn messages(&self, session_key: &SessionKey) -> Vec<Message> {
        let sessions = self.sessions.lock().expect("sessions mutex poisoned");
        sessions.get(session_key).cloned().unwrap_or_default()
    }

    async fn assistant_response(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let streaming = self.provider.supports_streaming();
        let span = info_span!(
            "provider_request",
            message_count = messages.len(),
            tool_count = tool_defs.len(),
            streaming,
        );

        async {
            if streaming {
                self.assistant_response_streaming(system_prompt, messages, tool_defs, event_tx)
                    .await
            } else {
                self.assistant_response_non_streaming(system_prompt, messages, tool_defs, event_tx)
                    .await
            }
        }
        .instrument(span)
        .await
    }

    async fn assistant_response_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let mut stream = self
            .provider
            .stream(system_prompt, messages, tool_defs)
            .await?;

        let mut response = Message::assistant();
        let mut usage = Usage::default();

        while let Some(item) = stream.next().await {
            let (msg_opt, usage_opt) = item?;

            if let Some(msg) = msg_opt {
                if let Some(final_usage) = usage_opt {
                    usage += final_usage;
                    response = msg;
                } else {
                    let text = msg.text();
                    if !text.is_empty() {
                        let _ = event_tx.send(TurnEvent::TextDelta(text)).await;
                    }
                }
            }
        }

        info!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
            "provider response complete"
        );

        Ok((response, usage))
    }

    async fn assistant_response_non_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let (response, usage) = self
            .provider
            .complete(system_prompt, messages, tool_defs)
            .await?;

        let text = response.text();
        if !text.is_empty() {
            let _ = event_tx.send(TurnEvent::TextDelta(text)).await;
        }

        info!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
            "provider response complete"
        );

        Ok((response, usage))
    }
}

fn parse_session_key(session: &str, agent_id: &str) -> Option<SessionKey> {
    if session == "main" {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Main,
        });
    }

    let rest = session.strip_prefix(&format!("{agent_id}:"))?;
    if rest == "main" {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Main,
        });
    }

    if let Some(dm) = rest.strip_prefix("dm:") {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Dm(dm.to_owned()),
        });
    }

    if let Some(group) = rest.strip_prefix("group:") {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Group(group.to_owned()),
        });
    }

    if let Some(isolated) = rest.strip_prefix("isolated:") {
        let uuid = Uuid::parse_str(isolated).ok()?;
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Isolated(uuid),
        });
    }

    None
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use coop_core::fakes::FakeProvider;
    use coop_core::tools::DefaultExecutor;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::config::Config;

    struct RecordingTypingNotifier {
        events: tokio::sync::Mutex<Vec<bool>>,
        notify: tokio::sync::Notify,
    }

    impl RecordingTypingNotifier {
        fn new() -> Self {
            Self {
                events: tokio::sync::Mutex::new(Vec::new()),
                notify: tokio::sync::Notify::new(),
            }
        }

        async fn wait_for_events(&self, count: usize) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            loop {
                if self.events.lock().await.len() >= count {
                    return;
                }

                let now = tokio::time::Instant::now();
                assert!(now < deadline, "timed out waiting for typing events");
                let wait = deadline.saturating_duration_since(now);
                let _ = tokio::time::timeout(wait, self.notify.notified()).await;
            }
        }
    }

    #[async_trait]
    impl TypingNotifier for RecordingTypingNotifier {
        async fn set_typing(&self, _session_key: &SessionKey, started: bool) {
            let mut events = self.events.lock().await;
            events.push(started);
            drop(events);
            self.notify.notify_waiters();
        }
    }

    fn test_config() -> Config {
        serde_yaml::from_str(
            "
agent:
  id: coop
  model: test-model
",
        )
        .unwrap()
    }

    #[test]
    fn parse_main_alias() {
        let key = parse_session_key("main", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Main,
            }
        );
    }

    #[test]
    fn parse_dm_session() {
        let key = parse_session_key("coop:dm:signal:alice-uuid", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
            }
        );
    }

    #[test]
    fn parse_group_session() {
        let key = parse_session_key("coop:group:signal:group:deadbeef", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Group("signal:group:deadbeef".to_owned()),
            }
        );
    }

    #[test]
    fn parse_rejects_other_agent() {
        assert!(parse_session_key("other:main", "coop").is_none());
    }

    fn test_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "You are a test agent.").unwrap();
        dir
    }

    #[tokio::test]
    async fn typing_guard_sends_stop_on_drop() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
        let executor: Arc<dyn ToolExecutor> = Arc::new(DefaultExecutor::new());
        let notifier: Arc<RecordingTypingNotifier> = Arc::new(RecordingTypingNotifier::new());
        let typing_notifier: Arc<dyn TypingNotifier> = Arc::clone(&notifier) as _;

        let gateway = Gateway::new(
            test_config(),
            workspace.path().to_path_buf(),
            provider,
            executor,
            Some(typing_notifier),
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(16);

        gateway
            .run_turn_with_trust(&session_key, "hello", TrustLevel::Full, event_tx)
            .await
            .unwrap();

        notifier.wait_for_events(2).await;
        let events = notifier.events.lock().await.clone();
        assert!(events[0]);
        assert!(!events[1]);
    }
}
