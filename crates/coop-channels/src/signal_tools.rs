use anyhow::Result;
use async_trait::async_trait;
use coop_core::{Tool, ToolContext, ToolDef, ToolExecutor, ToolOutput};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span};

use crate::signal::{SignalAction, SignalTarget};

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

#[allow(missing_debug_implementations)]
pub struct SignalToolExecutor {
    tools: Vec<Box<dyn Tool>>,
}

impl SignalToolExecutor {
    pub fn new(action_tx: mpsc::Sender<SignalAction>) -> Self {
        Self {
            tools: vec![
                Box::new(SignalReactTool::new(action_tx.clone())),
                Box::new(SignalReplyTool::new(action_tx)),
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
}
