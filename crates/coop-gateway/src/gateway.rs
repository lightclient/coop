use anyhow::Result;
use coop_core::{
    Message, Provider, SessionKey, SessionKind, ToolContext, ToolDef, ToolExecutor, TrustLevel,
    TurnConfig, TurnEvent, TurnResult, Usage,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span};
use uuid::Uuid;

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
        keys.push(self.default_session_key());
        keys.sort_by_cached_key(ToString::to_string);
        keys.dedup_by(|a, b| a.to_string() == b.to_string());
        keys
    }

    pub(crate) fn resolve_session(&self, session: &str) -> Option<SessionKey> {
        parse_session_key(session, &self.config.agent.id)
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
        let span = info_span!(
            "agent_turn",
            session = %session_key,
            input_len = user_input.len(),
            trust = ?trust,
        );

        async {
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
                        .assistant_response(&messages, &tool_defs, &event_tx)
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
        let streaming = self.provider.supports_streaming();
        let span = info_span!(
            "provider_request",
            message_count = messages.len(),
            tool_count = tool_defs.len(),
            streaming,
        );

        async {
            if streaming {
                self.assistant_response_streaming(messages, tool_defs, event_tx)
                    .await
            } else {
                self.assistant_response_non_streaming(messages, tool_defs, event_tx)
                    .await
            }
        }
        .instrument(span)
        .await
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

        info!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            "provider response complete"
        );

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

        info!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            "provider response complete"
        );

        Ok((response, usage))
    }
}

fn parse_session_key(session: &str, agent_id: &str) -> Option<SessionKey> {
    if session == "main" {
        return Some(SessionKey {
            agent_id: agent_id.to_string(),
            kind: SessionKind::Main,
        });
    }

    let rest = session.strip_prefix(&format!("{agent_id}:"))?;
    if rest == "main" {
        return Some(SessionKey {
            agent_id: agent_id.to_string(),
            kind: SessionKind::Main,
        });
    }

    if let Some(dm) = rest.strip_prefix("dm:") {
        return Some(SessionKey {
            agent_id: agent_id.to_string(),
            kind: SessionKind::Dm(dm.to_string()),
        });
    }

    if let Some(group) = rest.strip_prefix("group:") {
        return Some(SessionKey {
            agent_id: agent_id.to_string(),
            kind: SessionKind::Group(group.to_string()),
        });
    }

    if let Some(isolated) = rest.strip_prefix("isolated:") {
        let uuid = Uuid::parse_str(isolated).ok()?;
        return Some(SessionKey {
            agent_id: agent_id.to_string(),
            kind: SessionKind::Isolated(uuid),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_main_alias() {
        let key = parse_session_key("main", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_string(),
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
                agent_id: "coop".to_string(),
                kind: SessionKind::Dm("signal:alice-uuid".to_string()),
            }
        );
    }

    #[test]
    fn parse_group_session() {
        let key = parse_session_key("coop:group:signal:group:deadbeef", "coop").unwrap();
        assert_eq!(
            key,
            SessionKey {
                agent_id: "coop".to_string(),
                kind: SessionKind::Group("signal:group:deadbeef".to_string()),
            }
        );
    }

    #[test]
    fn parse_rejects_other_agent() {
        assert!(parse_session_key("other:main", "coop").is_none());
    }
}
