use anyhow::Result;
use coop_core::{
    Message, Provider, SessionKey, SessionKind, ToolContext, ToolDef, ToolExecutor, TrustLevel,
    TurnConfig, TurnEvent, TurnResult, Usage,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::error;

use crate::config::Config;

pub(crate) struct Gateway {
    config: Config,
    system_prompt: String,
    provider: Arc<dyn Provider>,
    executor: Arc<dyn ToolExecutor>,
    sessions: Mutex<HashMap<SessionKey, Vec<Message>>>,
}

impl Gateway {
    pub(crate) fn new(
        config: Config,
        system_prompt: String,
        provider: Arc<dyn Provider>,
        executor: Arc<dyn ToolExecutor>,
    ) -> Self {
        Self {
            config,
            system_prompt,
            provider,
            executor,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn default_session_key(&self) -> SessionKey {
        SessionKey {
            agent_id: self.config.agent.id.clone(),
            kind: SessionKind::Main,
        }
    }

    pub(crate) fn list_sessions(&self) -> Vec<SessionKey> {
        let sessions = self.sessions.lock().unwrap();
        let mut keys: Vec<_> = sessions.keys().cloned().collect();
        keys.sort_by_cached_key(ToString::to_string);
        keys
    }

    pub(crate) fn find_session(&self, session: &str) -> Option<SessionKey> {
        if session == "main" {
            return Some(self.default_session_key());
        }

        let sessions = self.sessions.lock().unwrap();
        sessions
            .keys()
            .find(|key| key.to_string() == session)
            .cloned()
    }

    fn tool_context(&self, session_key: &SessionKey, trust: TrustLevel) -> ToolContext {
        let workspace = std::path::PathBuf::from(&self.config.agent.workspace);
        let workspace = if workspace.is_relative() {
            std::env::current_dir().unwrap_or_default().join(workspace)
        } else {
            workspace
        };

        ToolContext {
            session_id: session_key.to_string(),
            trust,
            workspace,
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
        self.append_message(session_key, Message::user().with_text(user_input));

        let tool_defs = self.executor.tools();
        let ctx = self.tool_context(session_key, trust);
        let turn_config = TurnConfig::default();

        let mut total_usage = Usage::default();
        let mut new_messages = Vec::new();
        let mut hit_limit = false;

        for iteration in 0..turn_config.max_iterations {
            let messages = self.messages(session_key);
            let (response, usage) = self
                .assistant_response(&messages, &tool_defs, &event_tx)
                .await?;

            total_usage += usage;
            self.append_message(session_key, response.clone());
            new_messages.push(response.clone());

            let _ = event_tx
                .send(TurnEvent::AssistantMessage(response.clone()))
                .await;

            if !response.has_tool_requests() {
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

                let output = match self
                    .executor
                    .execute(&req.name, req.arguments.clone(), &ctx)
                    .await
                {
                    Ok(output) => output,
                    Err(err) => {
                        error!(tool = %req.name, error = %err, "tool execution failed");
                        coop_core::ToolOutput::error(format!("internal error: {err}"))
                    }
                };

                result_msg = result_msg.with_tool_result(&req.id, &output.content, output.is_error);

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

        let _ = event_tx
            .send(TurnEvent::Done(TurnResult {
                messages: new_messages,
                usage: total_usage,
                hit_limit,
            }))
            .await;

        Ok(())
    }

    pub(crate) fn clear_session(&self, session_key: &SessionKey) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(session_key);
    }

    fn append_message(&self, session_key: &SessionKey, message: Message) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions
            .entry(session_key.clone())
            .or_default()
            .push(message);
    }

    fn messages(&self, session_key: &SessionKey) -> Vec<Message> {
        let sessions = self.sessions.lock().unwrap();
        sessions.get(session_key).cloned().unwrap_or_default()
    }

    async fn assistant_response(
        &self,
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        if self.provider.supports_streaming() {
            self.assistant_response_streaming(messages, tool_defs, event_tx)
                .await
        } else {
            self.assistant_response_non_streaming(messages, tool_defs, event_tx)
                .await
        }
    }

    async fn assistant_response_streaming(
        &self,
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let mut stream = self
            .provider
            .stream(&self.system_prompt, messages, tool_defs)
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

        Ok((response, usage))
    }

    async fn assistant_response_non_streaming(
        &self,
        messages: &[Message],
        tool_defs: &[ToolDef],
        event_tx: &mpsc::Sender<TurnEvent>,
    ) -> Result<(Message, Usage)> {
        let (response, usage) = self
            .provider
            .complete(&self.system_prompt, messages, tool_defs)
            .await?;

        let text = response.text();
        if !text.is_empty() {
            let _ = event_tx.send(TurnEvent::TextDelta(text)).await;
        }

        Ok((response, usage))
    }
}
