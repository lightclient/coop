use anyhow::Result;
use coop_core::prompt::{
    PromptBuilder, SkillEntry, WorkspaceIndex, default_file_configs, scan_skills,
};
use coop_core::{
    InboundMessage, Message, Provider, SessionKey, SessionKind, ToolContext, ToolDef, ToolExecutor,
    TrustLevel, TurnConfig, TurnEvent, TurnResult, TypingNotifier, Usage,
};
use coop_memory::{Memory, NewObservation, min_trust_for_store, trust_to_store};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, info_span, warn};
use uuid::Uuid;

use crate::config::Config;
use crate::session_store::DiskSessionStore;

pub(crate) struct Gateway {
    config: Config,
    workspace: PathBuf,
    workspace_index: Mutex<WorkspaceIndex>,
    skills: Vec<SkillEntry>,
    provider: Arc<dyn Provider>,
    executor: Arc<dyn ToolExecutor>,
    memory: Option<Arc<dyn Memory>>,
    typing_notifier: Option<Arc<dyn TypingNotifier>>,
    sessions: Mutex<HashMap<SessionKey, Vec<Message>>>,
    session_store: DiskSessionStore,
}

/// Re-send interval for typing indicators. Signal's client-side timeout is
/// ~10 s, so we refresh well within that window.
const TYPING_REFRESH_INTERVAL: Duration = Duration::from_secs(8);

struct TypingGuard {
    cancel: CancellationToken,
}

impl TypingGuard {
    fn new(notifier: Arc<dyn TypingNotifier>, session_key: SessionKey) -> Self {
        let cancel = CancellationToken::new();
        let child = cancel.child_token();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(TYPING_REFRESH_INTERVAL) => {
                        info!(
                            session = %session_key,
                            "typing notifier refresh",
                        );
                        notifier.set_typing(&session_key, true).await;
                    }
                    () = child.cancelled() => {
                        emit_typing_notifier_event(&session_key, false);
                        notifier.set_typing(&session_key, false).await;
                        break;
                    }
                }
            }
        });

        Self { cancel }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
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
        SessionKind::Main | SessionKind::Isolated(_) | SessionKind::Cron(_) => None,
    }
}

impl Gateway {
    pub(crate) fn new(
        config: Config,
        workspace: PathBuf,
        provider: Arc<dyn Provider>,
        executor: Arc<dyn ToolExecutor>,
        typing_notifier: Option<Arc<dyn TypingNotifier>>,
        memory: Option<Arc<dyn Memory>>,
    ) -> Result<Self> {
        let file_configs = default_file_configs();
        let workspace_index = WorkspaceIndex::scan(&workspace, &file_configs)?;
        let skills = scan_skills(&workspace);
        if !skills.is_empty() {
            debug!(count = skills.len(), "loaded skills");
        }

        let session_store = DiskSessionStore::new(workspace.join("sessions"))?;

        Ok(Self {
            config,
            workspace,
            workspace_index: Mutex::new(workspace_index),
            skills,
            provider,
            executor,
            memory,
            typing_notifier,
            sessions: Mutex::new(HashMap::new()),
            session_store,
        })
    }

    /// Build a trust-gated system prompt for this turn.
    fn build_prompt(&self, trust: TrustLevel, user_name: Option<&str>) -> Result<String> {
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

        let mut builder = PromptBuilder::new(self.workspace.clone(), self.config.agent.id.clone())
            .trust(trust)
            .model(&self.config.agent.model)
            .skills(self.skills.clone());
        if let Some(name) = user_name {
            builder = builder.user(name);
        }
        let prompt = builder.build(&index)?;
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

    fn tool_context(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        user_name: Option<&str>,
    ) -> ToolContext {
        ToolContext {
            session_id: session_key.to_string(),
            trust,
            workspace: self.workspace.clone(),
            user_name: user_name.map(str::to_owned),
        }
    }

    fn capture_tool_observation(
        &self,
        session_key: &SessionKey,
        trust: TrustLevel,
        tool_name: &str,
        arguments: &serde_json::Value,
        output: &coop_core::ToolOutput,
    ) {
        let Some(memory) = self.memory.as_ref().map(Arc::clone) else {
            return;
        };

        if tool_name.starts_with("memory_") {
            return;
        }

        let args = serde_json::to_string(arguments).unwrap_or_default();
        let mut output_text = output.content.clone();
        if output_text.len() > 1200 {
            output_text.truncate(1200);
            output_text.push_str("... [truncated]");
        }

        let mut related_files = Vec::new();
        for key in ["path", "file", "target", "from", "to"] {
            if let Some(path) = arguments.get(key).and_then(serde_json::Value::as_str) {
                related_files.push(path.to_owned());
            }
        }

        let store = trust_to_store(trust).to_owned();
        let min_trust = min_trust_for_store(&store);

        let tool_name_owned = tool_name.to_owned();
        let obs = NewObservation {
            session_key: Some(session_key.to_string()),
            store,
            obs_type: "technical".to_owned(),
            title: format!("Tool run: {tool_name}"),
            narrative: format!("arguments={args}\noutput={output_text}"),
            facts: vec![
                format!("tool={tool_name}"),
                format!("error={}", output.is_error),
            ],
            tags: vec!["tool".to_owned(), tool_name.to_owned()],
            source: "auto".to_owned(),
            related_files,
            related_people: Vec::new(),
            token_count: None,
            expires_at: None,
            min_trust,
        };

        tokio::spawn(async move {
            match memory.write(obs).await {
                Ok(outcome) => {
                    debug!(?outcome, tool = %tool_name_owned, "auto-captured tool observation");
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        tool = %tool_name_owned,
                        "failed to auto-capture tool observation"
                    );
                }
            }
        });
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_turn_with_trust(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        trust: TrustLevel,
        user_name: Option<&str>,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        let span = info_span!(
            "agent_turn",
            session = %session_key,
            input_len = user_input.len(),
            user_input = user_input,
            trust = ?trust,
            user = ?user_name,
        );

        async {
            let _typing_guard = if let Some(notifier) = &self.typing_notifier {
                emit_typing_notifier_event(session_key, true);
                notifier.set_typing(session_key, true).await;
                Some(TypingGuard::new(Arc::clone(notifier), session_key.clone()))
            } else {
                None
            };

            let system_prompt = self.build_prompt(trust, user_name)?;
            let session_len_before = self.messages(session_key).len();
            self.append_message(session_key, Message::user().with_text(user_input));

            let tool_defs = self.executor.tools();
            let ctx = self.tool_context(session_key, trust, user_name);
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

                let iter_result: Result<(Message, bool)> = async {
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
                    Ok((response, !has_tool_requests))
                }
                .instrument(iter_span)
                .await;

                let (response, should_break) = match iter_result {
                    Ok(result) => result,
                    Err(err) => {
                        let session_len_now = self.messages(session_key).len();
                        let rolled_back = session_len_now - session_len_before;
                        error!(
                            error = %err,
                            iteration,
                            trust = ?trust,
                            messages_rolled_back = rolled_back,
                            "provider request failed, rolling back session"
                        );
                        self.truncate_session(session_key, session_len_before);
                        let user_msg = if trust == TrustLevel::Full {
                            format!("{err:#}")
                        } else {
                            "Something went wrong. Please try again later.".to_owned()
                        };
                        let _ = event_tx.send(TurnEvent::Error(user_msg)).await;
                        let _ = event_tx
                            .send(TurnEvent::Done(TurnResult {
                                messages: new_messages,
                                usage: total_usage,
                                hit_limit: false,
                            }))
                            .await;
                        return Ok(());
                    }
                };

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

                    self.capture_tool_observation(
                        session_key,
                        trust,
                        &req.name,
                        &req.arguments,
                        &output,
                    );
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
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .remove(session_key);
        if let Err(e) = self.session_store.delete(session_key) {
            warn!(session = %session_key, error = %e, "failed to delete persisted session");
        }
    }

    fn append_message(&self, session_key: &SessionKey, message: Message) {
        if let Err(e) = self.session_store.append(session_key, &message) {
            warn!(session = %session_key, error = %e, "failed to persist message");
        }
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        sessions
            .entry(session_key.clone())
            .or_default()
            .push(message);
    }

    /// Truncate session history back to `len` messages.
    fn truncate_session(&self, session_key: &SessionKey, len: usize) {
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        if let Some(msgs) = sessions.get_mut(session_key) {
            msgs.truncate(len);
            if let Err(e) = self.session_store.replace(session_key, msgs) {
                warn!(session = %session_key, error = %e, "failed to persist truncated session");
            }
        }
    }

    fn messages(&self, session_key: &SessionKey) -> Vec<Message> {
        let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
        if !sessions.contains_key(session_key) {
            match self.session_store.load(session_key) {
                Ok(msgs) if !msgs.is_empty() => {
                    sessions.insert(session_key.clone(), msgs);
                }
                Err(e) => {
                    warn!(session = %session_key, error = %e, "failed to load session from disk");
                }
                _ => {}
            }
        }
        sessions.get(session_key).cloned().unwrap_or_default()
    }

    /// Returns true if a session has no messages (checks disk too).
    #[allow(dead_code)]
    pub(crate) fn session_is_empty(&self, session_key: &SessionKey) -> bool {
        self.messages(session_key).is_empty()
    }

    /// Seed a session with formatted Signal chat history for context.
    #[allow(dead_code)]
    pub(crate) fn seed_signal_history(&self, session_key: &SessionKey, history: &[InboundMessage]) {
        if history.is_empty() {
            return;
        }

        let mut context = String::from("[Recent Signal chat history for context]\n");
        for msg in history {
            context.push_str(&msg.content);
            context.push('\n');
        }
        context.push_str("[End of history context]");

        info!(
            session = %session_key,
            message_count = history.len(),
            "seeding session with signal history"
        );

        self.append_message(session_key, Message::user().with_text(context));
        self.append_message(
            session_key,
            Message::assistant().with_text("I have context from the recent conversation history."),
        );
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

    if let Some(cron_name) = rest.strip_prefix("cron:")
        && !cron_name.is_empty()
    {
        return Some(SessionKey {
            agent_id: agent_id.to_owned(),
            kind: SessionKind::Cron(cron_name.to_owned()),
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
    use coop_core::traits::ProviderStream;
    use coop_core::types::ModelInfo;
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
    fn parse_cron_session() {
        let key = parse_session_key("coop:cron:heartbeat", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Cron("heartbeat".to_owned()),
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

    /// Provider that always fails with the given error message.
    #[derive(Debug)]
    struct FailingProvider {
        error_msg: String,
        model: ModelInfo,
    }

    impl FailingProvider {
        fn new(msg: impl Into<String>) -> Self {
            Self {
                error_msg: msg.into(),
                model: ModelInfo {
                    name: "fail-model".into(),
                    context_limit: 128_000,
                },
            }
        }
    }

    #[async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn model_info(&self) -> &ModelInfo {
            &self.model
        }

        async fn complete(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            anyhow::bail!("{}", self.error_msg)
        }

        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("{}", self.error_msg)
        }
    }

    /// Provider that succeeds on the first call (returning a tool_use),
    /// then fails on the second call.
    #[derive(Debug)]
    struct FailOnSecondCallProvider {
        model: ModelInfo,
        call_count: Mutex<u32>,
        error_msg: String,
    }

    impl FailOnSecondCallProvider {
        fn new(msg: impl Into<String>) -> Self {
            Self {
                model: ModelInfo {
                    name: "fail-second".into(),
                    context_limit: 128_000,
                },
                call_count: Mutex::new(0),
                error_msg: msg.into(),
            }
        }
    }

    #[async_trait]
    impl Provider for FailOnSecondCallProvider {
        fn name(&self) -> &'static str {
            "fail-second"
        }

        fn model_info(&self) -> &ModelInfo {
            &self.model
        }

        async fn complete(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<(Message, Usage)> {
            let mut count = self.call_count.lock().unwrap();
            *count += 1;
            if *count == 1 {
                // First call: return a tool_use so the turn continues
                Ok((
                    Message::assistant().with_tool_request(
                        "tool_1",
                        "bash",
                        serde_json::json!({"command": "echo hi"}),
                    ),
                    Usage {
                        input_tokens: Some(100),
                        output_tokens: Some(50),
                        ..Default::default()
                    },
                ))
            } else {
                anyhow::bail!("{}", self.error_msg)
            }
        }

        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ProviderStream> {
            anyhow::bail!("not supported")
        }
    }

    #[tokio::test]
    async fn provider_error_mid_turn_rolls_back_all_messages() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailOnSecondCallProvider::new(
            "Anthropic API error: 500 - internal server error",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                test_config(),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                event_tx,
            )
            .await;

        assert!(result.is_ok(), "should not propagate error");

        let mut saw_error = false;
        let mut saw_tool_start = false;
        let mut saw_done = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(_) => saw_error = true,
                TurnEvent::ToolStart { .. } => saw_tool_start = true,
                TurnEvent::Done(_) => saw_done = true,
                _ => {}
            }
        }

        assert!(saw_tool_start, "tool should have executed on iteration 0");
        assert!(saw_error, "should emit error on iteration 1 failure");
        assert!(saw_done, "should emit Done after error");

        // Session must be fully rolled back — no user msg, no assistant msg,
        // no tool result from the partial turn.
        assert!(
            gateway.messages(&session_key).is_empty(),
            "session should be fully rolled back after mid-turn error"
        );
    }

    #[tokio::test]
    async fn provider_error_sends_detail_to_full_trust_user() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new(
            "Anthropic API error: 400 - bad request",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                test_config(),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                event_tx,
            )
            .await;

        assert!(result.is_ok(), "should not propagate error");

        let mut saw_error = false;
        let mut error_msg = String::new();
        let mut saw_done = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                TurnEvent::Error(msg) => {
                    saw_error = true;
                    error_msg = msg;
                }
                TurnEvent::Done(_) => saw_done = true,
                _ => {}
            }
        }

        assert!(saw_error, "should emit TurnEvent::Error");
        assert!(
            error_msg.contains("400"),
            "full-trust user should see actual error: {error_msg}"
        );
        assert!(saw_done, "should emit TurnEvent::Done after error");

        // Session should be rolled back — no leftover user message
        assert!(
            gateway.messages(&session_key).is_empty(),
            "session should be rolled back on error"
        );
    }

    #[tokio::test]
    async fn provider_error_hides_detail_from_public_trust_user() {
        let workspace = test_workspace();
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new(
            "Anthropic API error: 400 - bad request",
        ));
        let executor = Arc::new(DefaultExecutor::new());
        let gateway = Arc::new(
            Gateway::new(
                test_config(),
                workspace.path().to_path_buf(),
                provider,
                executor,
                None,
                None,
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        let (event_tx, mut event_rx) = mpsc::channel(32);

        let result = gateway
            .run_turn_with_trust(&session_key, "hello", TrustLevel::Public, None, event_tx)
            .await;

        assert!(result.is_ok());

        let mut error_msg = String::new();
        while let Ok(event) = event_rx.try_recv() {
            if let TurnEvent::Error(msg) = event {
                error_msg = msg;
            }
        }

        assert!(
            !error_msg.contains("400"),
            "public user should NOT see API details: {error_msg}"
        );
        assert!(
            error_msg.contains("Something went wrong"),
            "public user should get generic message: {error_msg}"
        );
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
            None,
        )
        .unwrap();

        let session_key = gateway.default_session_key();
        let (event_tx, _event_rx) = mpsc::channel(16);

        gateway
            .run_turn_with_trust(
                &session_key,
                "hello",
                TrustLevel::Full,
                Some("alice"),
                event_tx,
            )
            .await
            .unwrap();

        notifier.wait_for_events(2).await;
        let events = notifier.events.lock().await.clone();
        assert!(events[0]);
        assert!(!events[1]);
    }

    #[tokio::test(start_paused = true)]
    async fn typing_guard_refreshes_periodically() {
        let notifier = Arc::new(RecordingTypingNotifier::new());
        let session_key = SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Main,
        };

        // Initial set_typing(true) as the callsite does
        notifier.set_typing(&session_key, true).await;
        let guard = TypingGuard::new(
            Arc::clone(&notifier) as Arc<dyn TypingNotifier>,
            session_key,
        );

        // Let the spawned background task register its first sleep.
        tokio::task::yield_now().await;

        // Advance past one refresh interval and let the task fully
        // re-enter its loop before advancing again.
        tokio::time::advance(TYPING_REFRESH_INTERVAL).await;
        // Yield a few times so the background task processes the
        // wakeup, calls set_typing, and re-registers its next sleep.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        let events = notifier.events.lock().await.clone();
        assert_eq!(events, vec![true, true], "after first refresh");

        tokio::time::advance(TYPING_REFRESH_INTERVAL).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        // initial true + 2 refresh trues = 3 events
        let events = notifier.events.lock().await.clone();
        assert_eq!(events, vec![true, true, true], "after second refresh");

        // Drop the guard → cancellation triggers stop
        drop(guard);
        tokio::task::yield_now().await;

        let events = notifier.events.lock().await.clone();
        assert_eq!(events, vec![true, true, true, false]);
    }
}
