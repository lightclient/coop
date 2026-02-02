use anyhow::Result;
use coop_core::{
    Message, Provider, SessionKey, SessionKind, ToolContext, ToolExecutor, TrustLevel, TurnConfig,
    TurnEvent, TurnResult, Usage,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::error;

use crate::config::Config;

/// Core gateway: manages sessions and routes messages to the agent runtime.
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

    /// The default session key for the configured agent.
    pub(crate) fn default_session_key(&self) -> SessionKey {
        SessionKey {
            agent_id: self.config.agent.id.clone(),
            kind: SessionKind::Main,
        }
    }

    fn tool_context(&self, session_key: &SessionKey) -> ToolContext {
        let workspace = std::path::PathBuf::from(&self.config.agent.workspace);
        let workspace = if workspace.is_relative() {
            std::env::current_dir().unwrap_or_default().join(workspace)
        } else {
            workspace
        };
        ToolContext {
            session_id: session_key.to_string(),
            trust: TrustLevel::Full,
            workspace,
        }
    }

    /// Run a full agent turn: provider calls + tool execution loop.
    ///
    /// Streams `TurnEvent`s back to the caller via the channel.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run_turn(
        &self,
        session_key: &SessionKey,
        user_input: &str,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<()> {
        // Append user message to session
        {
            let mut sessions = self.sessions.lock().unwrap();
            sessions
                .entry(session_key.clone())
                .or_default()
                .push(Message::user().with_text(user_input));
        }

        let tool_defs = self.executor.tools();
        let ctx = self.tool_context(session_key);
        let turn_config = TurnConfig::default();
        let mut total_usage = Usage::default();
        let mut new_messages = Vec::new();
        let mut hit_limit = false;

        for iteration in 0..turn_config.max_iterations {
            let messages = {
                let sessions = self.sessions.lock().unwrap();
                sessions.get(session_key).cloned().unwrap_or_default()
            };

            let mut stream = self
                .provider
                .stream(&self.system_prompt, &messages, &tool_defs)
                .await?;

            let mut response = Message::assistant();
            while let Some(item) = stream.next().await {
                let (msg_opt, usage_opt) = item?;
                if let Some(msg) = msg_opt {
                    if let Some(usage) = usage_opt {
                        // Final message with usage — this is the complete response
                        total_usage += usage;
                        response = msg;
                    } else {
                        // Partial delta — stream text to TUI
                        let text = msg.text();
                        if !text.is_empty() {
                            let _ = event_tx.send(TurnEvent::TextDelta(text)).await;
                        }
                    }
                }
            }

            // Append assistant message to session
            {
                let mut sessions = self.sessions.lock().unwrap();
                sessions
                    .entry(session_key.clone())
                    .or_default()
                    .push(response.clone());
            }
            new_messages.push(response.clone());

            let _ = event_tx
                .send(TurnEvent::AssistantMessage(response.clone()))
                .await;

            if !response.has_tool_requests() {
                break;
            }

            let tool_requests = response.tool_requests();

            // Build a user message with all tool results
            let mut result_msg = Message::user();

            for req in &tool_requests {
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
                    Err(e) => {
                        error!(tool = %req.name, error = %e, "tool execution failed");
                        coop_core::ToolOutput::error(format!("internal error: {e}"))
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

            // Append tool results to session
            {
                let mut sessions = self.sessions.lock().unwrap();
                sessions
                    .entry(session_key.clone())
                    .or_default()
                    .push(result_msg.clone());
            }
            new_messages.push(result_msg);

            // Check if this was the last iteration
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

    /// Clear a session.
    #[allow(dead_code)]
    pub(crate) fn clear_session(&self, session_key: &SessionKey) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.remove(session_key);
    }
}
