use coop_core::{Content, Message, TurnEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { version: u32 },
    Send { session: String, content: String },
    Clear { session: String },
    ListSessions,
    Subscribe { session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        version: u32,
        agent_id: String,
    },
    TextDelta {
        session: String,
        text: String,
    },
    ToolStart {
        session: String,
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        session: String,
        id: String,
        output: String,
        is_error: bool,
    },
    AssistantMessage {
        session: String,
        text: String,
    },
    Done {
        session: String,
        tokens: u32,
        hit_limit: bool,
    },
    Error {
        session: String,
        message: String,
    },
    Sessions {
        keys: Vec<String>,
    },
}

impl ServerMessage {
    pub fn from_turn_event(session: impl Into<String>, event: TurnEvent) -> Option<Self> {
        let session = session.into();

        match event {
            TurnEvent::TextDelta(text) => Some(Self::TextDelta { session, text }),
            TurnEvent::AssistantMessage(msg) => Some(Self::AssistantMessage {
                session,
                text: msg.text(),
            }),
            TurnEvent::ToolStart {
                id,
                name,
                arguments,
            } => Some(Self::ToolStart {
                session,
                id,
                name,
                arguments,
            }),
            TurnEvent::ToolResult { id, message } => {
                let (output, is_error) = tool_result_payload(&message);
                Some(Self::ToolResult {
                    session,
                    id,
                    output,
                    is_error,
                })
            }
            TurnEvent::Done(result) => Some(Self::Done {
                session,
                tokens: result.usage.total_tokens(),
                hit_limit: result.hit_limit,
            }),
            TurnEvent::Error(message) => Some(Self::Error { session, message }),
        }
    }
}

fn tool_result_payload(message: &Message) -> (String, bool) {
    message
        .content
        .iter()
        .find_map(|content| match content {
            Content::ToolResult {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| (message.text(), false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::{Message, TurnResult, Usage};

    #[test]
    fn client_message_round_trip() {
        let message = ClientMessage::Send {
            session: "main".into(),
            content: "hello".into(),
        };
        let json = serde_json::to_string(&message).unwrap();
        let parsed: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, message);
    }

    #[test]
    fn server_message_round_trip() {
        let message = ServerMessage::Done {
            session: "main".into(),
            tokens: 42,
            hit_limit: false,
        };
        let json = serde_json::to_string(&message).unwrap();
        let parsed: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, message);
    }

    #[test]
    fn maps_tool_result_event() {
        let event = TurnEvent::ToolResult {
            id: "call_1".into(),
            message: Message::user().with_tool_result("call_1", "ok", false),
        };

        let mapped = ServerMessage::from_turn_event("main", event).unwrap();
        assert_eq!(
            mapped,
            ServerMessage::ToolResult {
                session: "main".into(),
                id: "call_1".into(),
                output: "ok".into(),
                is_error: false,
            }
        );
    }

    #[test]
    fn maps_done_event() {
        let event = TurnEvent::Done(TurnResult {
            messages: Vec::new(),
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                ..Default::default()
            },
            hit_limit: true,
        });

        let mapped = ServerMessage::from_turn_event("main", event).unwrap();
        assert_eq!(
            mapped,
            ServerMessage::Done {
                session: "main".into(),
                tokens: 30,
                hit_limit: true,
            }
        );
    }
}
