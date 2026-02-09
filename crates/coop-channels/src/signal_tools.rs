use anyhow::Result;
use async_trait::async_trait;
use coop_core::{Tool, ToolContext, ToolDef, ToolExecutor, ToolOutput};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span};

use crate::signal::{SignalAction, SignalQuery, SignalTarget};

#[derive(Debug)]
pub struct SignalReactTool {
    action_tx: mpsc::Sender<SignalAction>,
}

impl SignalReactTool {
    pub fn new(action_tx: mpsc::Sender<SignalAction>) -> Self {
        Self { action_tx }
    }
}

#[derive(Debug)]
pub struct SignalReplyTool {
    action_tx: mpsc::Sender<SignalAction>,
}

impl SignalReplyTool {
    pub fn new(action_tx: mpsc::Sender<SignalAction>) -> Self {
        Self { action_tx }
    }
}

#[derive(Debug, Deserialize)]
struct ReactArgs {
    chat_id: String,
    emoji: String,
    message_timestamp: u64,
    author_id: String,
    #[serde(default)]
    remove: bool,
}

#[derive(Debug, Deserialize)]
struct ReplyArgs {
    chat_id: String,
    text: String,
    reply_to_timestamp: u64,
    author_id: String,
}

#[async_trait]
impl Tool for SignalReactTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "signal_react",
            "React to a Signal message with an emoji",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": {
                        "type": "string",
                        "description": "Chat identifier, e.g. a UUID for DMs or group:hex for groups"
                    },
                    "emoji": {
                        "type": "string",
                        "description": "Emoji to react with"
                    },
                    "message_timestamp": {
                        "type": "integer",
                        "description": "Timestamp of the message to react to"
                    },
                    "author_id": {
                        "type": "string",
                        "description": "UUID of the message author"
                    },
                    "remove": {
                        "type": "boolean",
                        "description": "Remove the reaction instead of adding"
                    }
                },
                "required": ["chat_id", "emoji", "message_timestamp", "author_id"]
            }),
        )
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let span = info_span!("signal_tool_react");
        async {
            let args: ReactArgs = serde_json::from_value(arguments)?;
            let target = SignalTarget::parse(&args.chat_id)?;

            info!(
                tool.name = "signal_react",
                signal.action = "react",
                signal.chat_id = %args.chat_id.as_str(),
                signal.emoji = %args.emoji.as_str(),
                signal.target_sent_timestamp = args.message_timestamp,
                signal.target_author_aci = %args.author_id.as_str(),
                signal.remove = args.remove,
                "signal tool action queued"
            );

            let action = SignalAction::React {
                target,
                emoji: args.emoji,
                target_author_aci: args.author_id,
                target_sent_timestamp: args.message_timestamp,
                remove: args.remove,
            };

            debug!(action = ?action, "dispatching signal_react action");
            self.action_tx
                .send(action)
                .await
                .map_err(|_send_err| anyhow::anyhow!("signal action channel closed"))?;

            Ok(ToolOutput::success("reaction sent"))
        }
        .instrument(span)
        .await
    }
}

#[async_trait]
impl Tool for SignalReplyTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "signal_reply",
            "Reply to a specific Signal message (shows as a quote)",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_id": {
                        "type": "string",
                        "description": "Chat identifier"
                    },
                    "text": {
                        "type": "string",
                        "description": "Reply text"
                    },
                    "reply_to_timestamp": {
                        "type": "integer",
                        "description": "Timestamp of the message to reply to"
                    },
                    "author_id": {
                        "type": "string",
                        "description": "UUID of the message author being replied to"
                    }
                },
                "required": ["chat_id", "text", "reply_to_timestamp", "author_id"]
            }),
        )
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let span = info_span!("signal_tool_reply");
        async {
            let args: ReplyArgs = serde_json::from_value(arguments)?;
            let target = SignalTarget::parse(&args.chat_id)?;

            info!(
                tool.name = "signal_reply",
                signal.action = "reply",
                signal.chat_id = %args.chat_id.as_str(),
                signal.raw_content = %args.text.as_str(),
                signal.quote_timestamp = args.reply_to_timestamp,
                signal.quote_author_aci = %args.author_id.as_str(),
                "signal tool action queued"
            );

            let action = SignalAction::Reply {
                target,
                text: args.text,
                quote_timestamp: args.reply_to_timestamp,
                quote_author_aci: args.author_id,
            };

            debug!(action = ?action, "dispatching signal_reply action");
            self.action_tx
                .send(action)
                .await
                .map_err(|_send_err| anyhow::anyhow!("signal action channel closed"))?;

            Ok(ToolOutput::success("reply sent"))
        }
        .instrument(span)
        .await
    }
}

#[derive(Debug)]
pub struct SignalHistoryTool {
    query_tx: mpsc::Sender<SignalQuery>,
}

impl SignalHistoryTool {
    pub fn new(query_tx: mpsc::Sender<SignalQuery>) -> Self {
        Self { query_tx }
    }
}

#[derive(Debug, Deserialize)]
struct HistoryArgs {
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    after: Option<u64>,
    #[serde(default = "default_history_limit")]
    limit: usize,
    #[serde(default)]
    query: Option<String>,
}

fn default_history_limit() -> usize {
    20
}

/// Extract a `SignalTarget` from a session ID like `agent:dm:signal:uuid`
/// or `agent:group:signal:group:hex`.
fn extract_signal_target_from_session(session_id: &str) -> Option<SignalTarget> {
    if let Some((_, rest)) = session_id.split_once(":dm:signal:") {
        return Some(SignalTarget::Direct(rest.to_owned()));
    }
    if let Some((_, rest)) = session_id.split_once(":group:signal:") {
        return SignalTarget::parse(rest).ok();
    }
    None
}

#[async_trait]
impl Tool for SignalHistoryTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "signal_history",
            "Search message history in the current Signal conversation. Only works in Signal chat sessions. Returns recent messages, optionally filtered by time range or text.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "before": {
                        "type": "integer",
                        "description": "Only messages before this epoch-ms timestamp"
                    },
                    "after": {
                        "type": "integer",
                        "description": "Only messages after this epoch-ms timestamp"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max messages to return (default 20, max 50)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional text to filter messages by content"
                    }
                }
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let span = info_span!("signal_tool_history");
        async {
            let args: HistoryArgs = serde_json::from_value(arguments)?;
            let target = extract_signal_target_from_session(&ctx.session_id).ok_or_else(|| {
                anyhow::anyhow!("signal_history is only available in Signal chat sessions")
            })?;

            let limit = args.limit.min(50);

            info!(
                tool.name = "signal_history",
                signal.limit = limit,
                signal.before = ?args.before,
                signal.after = ?args.after,
                signal.query = ?args.query,
                "signal history query"
            );

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            self.query_tx
                .send(SignalQuery::RecentMessages {
                    target,
                    limit,
                    before: args.before,
                    after: args.after,
                    reply: reply_tx,
                })
                .await
                .map_err(|_send_err| anyhow::anyhow!("signal query channel closed"))?;

            let messages = reply_rx
                .await
                .map_err(|_recv_err| anyhow::anyhow!("signal query response lost"))??;

            // Filter by query text if provided
            let messages: Vec<_> = if let Some(query) = &args.query {
                let query_lower = query.to_lowercase();
                messages
                    .into_iter()
                    .filter(|msg| msg.content.to_lowercase().contains(&query_lower))
                    .collect()
            } else {
                messages
            };

            if messages.is_empty() {
                return Ok(ToolOutput::success("No messages found."));
            }

            let mut output = format!("Found {} messages:\n\n", messages.len());
            for msg in &messages {
                output.push_str(&msg.content);
                output.push('\n');
            }

            Ok(ToolOutput::success(output))
        }
        .instrument(span)
        .await
    }
}

#[derive(Debug)]
pub struct SignalSendTool {
    action_tx: mpsc::Sender<SignalAction>,
}

impl SignalSendTool {
    pub fn new(action_tx: mpsc::Sender<SignalAction>) -> Self {
        Self { action_tx }
    }
}

#[derive(Debug, Deserialize)]
struct SendArgs {
    text: String,
}

#[async_trait]
impl Tool for SignalSendTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "signal_send",
            "Send a message to the current Signal conversation immediately, mid-turn. Use this to notify the user before a long-running task. Your final turn reply is still delivered separately.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Message text to send"
                    }
                },
                "required": ["text"]
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let span = info_span!("signal_tool_send");
        async {
            let args: SendArgs = serde_json::from_value(arguments)?;

            // Derive the send target from the session ID (same logic as signal_history).
            let target = extract_signal_target_from_session(&ctx.session_id).ok_or_else(|| {
                anyhow::anyhow!("signal_send is only available in Signal chat sessions")
            })?;

            info!(
                tool.name = "signal_send",
                signal.action = "send",
                signal.raw_content = %args.text.as_str(),
                "signal tool send queued"
            );

            let action = SignalAction::SendText(coop_core::OutboundMessage {
                channel: "signal".to_owned(),
                target: match &target {
                    SignalTarget::Direct(uuid) => uuid.clone(),
                    SignalTarget::Group { master_key } => {
                        format!("group:{}", hex::encode(master_key))
                    }
                },
                content: args.text,
            });

            debug!(action = ?action, "dispatching signal_send action");
            self.action_tx
                .send(action)
                .await
                .map_err(|_send_err| anyhow::anyhow!("signal action channel closed"))?;

            Ok(ToolOutput::success("message sent"))
        }
        .instrument(span)
        .await
    }
}

#[allow(missing_debug_implementations)]
pub struct SignalToolExecutor {
    tools: Vec<Box<dyn Tool>>,
}

impl SignalToolExecutor {
    pub fn new(action_tx: mpsc::Sender<SignalAction>, query_tx: mpsc::Sender<SignalQuery>) -> Self {
        Self {
            tools: vec![
                Box::new(SignalReactTool::new(action_tx.clone())),
                Box::new(SignalReplyTool::new(action_tx.clone())),
                Box::new(SignalSendTool::new(action_tx)),
                Box::new(SignalHistoryTool::new(query_tx)),
            ],
        }
    }
}

#[async_trait]
impl ToolExecutor for SignalToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        for tool in &self.tools {
            if tool.definition().name == name {
                return tool.execute(arguments, ctx).await;
            }
        }
        Ok(ToolOutput::error(format!("unknown tool: {name}")))
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::TrustLevel;
    use std::path::PathBuf;

    const GROUP_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn context() -> ToolContext {
        ToolContext {
            session_id: "session".to_owned(),
            trust: TrustLevel::Full,
            workspace: PathBuf::from("."),
            user_name: None,
        }
    }

    fn react_args(chat_id: &str) -> serde_json::Value {
        serde_json::json!({
            "chat_id": chat_id,
            "emoji": "👍",
            "message_timestamp": 42,
            "author_id": "alice-uuid"
        })
    }

    fn reply_args(chat_id: &str) -> serde_json::Value {
        serde_json::json!({
            "chat_id": chat_id,
            "text": "hello",
            "reply_to_timestamp": 77,
            "author_id": "alice-uuid"
        })
    }

    #[tokio::test]
    async fn react_tool_sends_direct_action() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = SignalReactTool::new(tx);

        let result = tool
            .execute(react_args("alice-uuid"), &context())
            .await
            .unwrap();
        assert_eq!(result.content, "reaction sent");

        let action = rx.recv().await.unwrap();
        match action {
            SignalAction::React {
                target,
                emoji,
                target_author_aci,
                target_sent_timestamp,
                remove,
            } => {
                assert_eq!(target, SignalTarget::Direct("alice-uuid".to_owned()));
                assert_eq!(emoji, "👍");
                assert_eq!(target_author_aci, "alice-uuid");
                assert_eq!(target_sent_timestamp, 42);
                assert!(!remove);
            }
            _ => panic!("expected react action"),
        }
    }

    #[tokio::test]
    async fn react_tool_sends_group_action() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = SignalReactTool::new(tx);

        let result = tool
            .execute(react_args(&format!("group:{GROUP_HEX}")), &context())
            .await
            .unwrap();
        assert_eq!(result.content, "reaction sent");

        let action = rx.recv().await.unwrap();
        match action {
            SignalAction::React { target, .. } => {
                assert_eq!(
                    target,
                    SignalTarget::Group {
                        master_key: hex::decode(GROUP_HEX).unwrap(),
                    }
                );
            }
            _ => panic!("expected react action"),
        }
    }

    #[tokio::test]
    async fn reply_tool_sends_direct_action() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = SignalReplyTool::new(tx);

        let result = tool
            .execute(reply_args("alice-uuid"), &context())
            .await
            .unwrap();
        assert_eq!(result.content, "reply sent");

        let action = rx.recv().await.unwrap();
        match action {
            SignalAction::Reply {
                target,
                text,
                quote_timestamp,
                quote_author_aci,
            } => {
                assert_eq!(target, SignalTarget::Direct("alice-uuid".to_owned()));
                assert_eq!(text, "hello");
                assert_eq!(quote_timestamp, 77);
                assert_eq!(quote_author_aci, "alice-uuid");
            }
            _ => panic!("expected reply action"),
        }
    }

    #[tokio::test]
    async fn reply_tool_sends_group_action() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = SignalReplyTool::new(tx);

        let result = tool
            .execute(reply_args(&format!("group:{GROUP_HEX}")), &context())
            .await
            .unwrap();
        assert_eq!(result.content, "reply sent");

        let action = rx.recv().await.unwrap();
        match action {
            SignalAction::Reply { target, .. } => {
                assert_eq!(
                    target,
                    SignalTarget::Group {
                        master_key: hex::decode(GROUP_HEX).unwrap(),
                    }
                );
            }
            _ => panic!("expected reply action"),
        }
    }

    #[tokio::test]
    async fn react_tool_rejects_invalid_chat_id() {
        let (tx, _rx) = mpsc::channel(1);
        let tool = SignalReactTool::new(tx);

        let error = tool
            .execute(react_args("group:not-hex"), &context())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid group target key"));
    }

    #[tokio::test]
    async fn reply_tool_rejects_invalid_chat_id() {
        let (tx, _rx) = mpsc::channel(1);
        let tool = SignalReplyTool::new(tx);

        let error = tool
            .execute(reply_args("group:not-hex"), &context())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid group target key"));
    }

    #[tokio::test]
    async fn react_tool_errors_when_action_channel_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let tool = SignalReactTool::new(tx);

        let error = tool
            .execute(react_args("alice-uuid"), &context())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signal action channel closed"));
    }

    #[tokio::test]
    async fn reply_tool_errors_when_action_channel_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let tool = SignalReplyTool::new(tx);

        let error = tool
            .execute(reply_args("alice-uuid"), &context())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signal action channel closed"));
    }

    #[tokio::test]
    async fn executor_has_react_reply_and_history() {
        let (action_tx, _rx) = mpsc::channel(1);
        let (query_tx, _qrx) = mpsc::channel(1);
        let executor = SignalToolExecutor::new(action_tx, query_tx);
        let names: Vec<_> = executor.tools().iter().map(|t| t.name.clone()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"signal_react".to_owned()));
        assert!(names.contains(&"signal_reply".to_owned()));
        assert!(names.contains(&"signal_history".to_owned()));
    }

    #[tokio::test]
    async fn history_tool_extracts_target_from_session() {
        assert_eq!(
            extract_signal_target_from_session("coop:dm:signal:alice-uuid"),
            Some(SignalTarget::Direct("alice-uuid".to_owned()))
        );
        assert!(
            extract_signal_target_from_session("coop:group:signal:group:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff").is_some()
        );
        assert_eq!(extract_signal_target_from_session("coop:main"), None);
    }

    #[tokio::test]
    async fn history_tool_rejects_non_signal_session() {
        let (query_tx, _qrx) = mpsc::channel(1);
        let tool = SignalHistoryTool::new(query_tx);
        let ctx = context(); // session_id = "session" — not a signal session

        let error = tool.execute(serde_json::json!({}), &ctx).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only available in Signal chat sessions")
        );
    }
}
