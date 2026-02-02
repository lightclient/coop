//! Conversion between Coop and Goose types.
//!
//! This is the boundary layer. Coop's types are the source of truth;
//! Goose's types are an implementation detail of the provider backend.

use coop_core::{Content, Message, ModelInfo, Role, ToolDef, Usage};
use goose::conversation::message::{
    Message as GooseMessage, MessageContent as GooseContent, ToolRequest as GooseToolRequest,
    ToolResponse as GooseToolResponse,
};
use goose::model::ModelConfig;
use goose::providers::base::ProviderUsage as GooseProviderUsage;
use rmcp::model::{CallToolResult, Content as McpContent, Tool as McpTool};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Message: Coop → Goose
// ---------------------------------------------------------------------------

/// Convert a Coop message to a Goose message for sending to a provider.
pub(crate) fn to_goose_message(msg: &Message) -> GooseMessage {
    let mut goose_msg = match msg.role {
        Role::User => GooseMessage::user(),
        Role::Assistant => GooseMessage::assistant(),
    };

    for content in &msg.content {
        goose_msg = match content {
            Content::Text { text } => goose_msg.with_text(text),

            Content::Image { data, mime_type } => goose_msg.with_image(data, mime_type),

            Content::ToolRequest {
                id,
                name,
                arguments,
            } => {
                let params = rmcp::model::CallToolRequestParams {
                    meta: None,
                    name: std::borrow::Cow::Owned(name.clone()),
                    arguments: arguments.as_object().cloned(),
                    task: None,
                };
                goose_msg.with_tool_request(id, Ok(params))
            }

            Content::ToolResult {
                id,
                output,
                is_error,
            } => {
                let result = CallToolResult {
                    content: vec![McpContent::text(output)],
                    structured_content: None,
                    is_error: Some(*is_error),
                    meta: None,
                };
                goose_msg.with_tool_response(id, Ok(result))
            }

            Content::Thinking {
                thinking,
                signature,
            } => {
                goose_msg.with_content(GooseContent::Thinking(
                    goose::conversation::message::ThinkingContent {
                        thinking: thinking.clone(),
                        signature: signature.clone().unwrap_or_default(),
                    },
                ))
            }
        };
    }

    goose_msg
}

/// Convert a slice of Coop messages to Goose messages.
pub(crate) fn to_goose_messages(messages: &[Message]) -> Vec<GooseMessage> {
    messages.iter().map(to_goose_message).collect()
}

// ---------------------------------------------------------------------------
// Message: Goose → Coop
// ---------------------------------------------------------------------------

/// Convert a Goose message to a Coop message.
pub(crate) fn from_goose_message(goose_msg: &GooseMessage) -> Message {
    let role = match goose_msg.role {
        rmcp::model::Role::User => Role::User,
        rmcp::model::Role::Assistant => Role::Assistant,
    };

    let mut msg = match role {
        Role::User => Message::user(),
        Role::Assistant => Message::assistant(),
    };

    for content in &goose_msg.content {
        let coop_content = match content {
            GooseContent::Text(text) => Content::text(&text.text),

            GooseContent::Image(img) => Content::image(&img.data, &img.mime_type),

            GooseContent::ToolRequest(GooseToolRequest { id, tool_call, .. }) => {
                match tool_call {
                    Ok(params) => Content::tool_request(
                        id,
                        params.name.as_ref(),
                        params
                            .arguments
                            .as_ref()
                            .map_or(Value::Object(serde_json::Map::default()), |a| Value::Object(a.clone())),
                    ),
                    Err(e) => {
                        // Malformed tool call from provider — represent as text
                        Content::text(format!("[malformed tool call {id}: {e}]"))
                    }
                }
            }

            GooseContent::ToolResponse(GooseToolResponse {
                id, tool_result, ..
            }) => {
                let (output, is_error) = match tool_result {
                    Ok(result) => {
                        let text = result
                            .content
                            .iter()
                            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let is_error = result.is_error.unwrap_or(false);
                        (text, is_error)
                    }
                    Err(e) => (format!("Error: {e}"), true),
                };
                Content::tool_result(id, output, is_error)
            }

            GooseContent::Thinking(t) => Content::Thinking {
                thinking: t.thinking.clone(),
                signature: Some(t.signature.clone()),
            },

            GooseContent::RedactedThinking(_) => Content::Thinking {
                thinking: String::new(),
                signature: None,
            },

            // Goose-specific content types we don't model — convert to text markers
            GooseContent::ToolConfirmationRequest(r) => {
                Content::text(format!("[tool confirmation: {}]", r.tool_name))
            }
            GooseContent::ActionRequired(a) => {
                Content::text(format!("[action required: {:?}]", a.data))
            }
            GooseContent::FrontendToolRequest(_) => {
                Content::text("[frontend tool request]")
            }
            GooseContent::SystemNotification(n) => {
                Content::text(format!("[system: {}]", n.msg))
            }
        };

        msg.content.push(coop_content);
    }

    msg
}

// ---------------------------------------------------------------------------
// Tools: Coop → Goose/MCP
// ---------------------------------------------------------------------------

/// Convert a Coop ToolDef to an rmcp Tool for passing to a provider.
pub(crate) fn to_mcp_tool(def: &ToolDef) -> McpTool {
    let schema = def
        .parameters
        .as_object()
        .cloned()
        .unwrap_or_default();

    McpTool {
        name: std::borrow::Cow::Owned(def.name.clone()),
        title: None,
        description: Some(std::borrow::Cow::Owned(def.description.clone())),
        input_schema: std::sync::Arc::new(schema),
        output_schema: None,
        annotations: None,
        icons: None,
        meta: None,
    }
}

/// Convert Coop ToolDefs to MCP tools.
pub(crate) fn to_mcp_tools(defs: &[ToolDef]) -> Vec<McpTool> {
    defs.iter().map(to_mcp_tool).collect()
}

// ---------------------------------------------------------------------------
// Usage: Goose → Coop
// ---------------------------------------------------------------------------

/// Convert Goose provider usage to Coop usage.
pub(crate) fn from_goose_usage(goose: &GooseProviderUsage) -> Usage {
    #[allow(clippy::cast_sign_loss)]
    Usage {
        input_tokens: goose.usage.input_tokens.map(|t| t as u32),
        output_tokens: goose.usage.output_tokens.map(|t| t as u32),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Model info: Goose → Coop
// ---------------------------------------------------------------------------

/// Extract Coop ModelInfo from a Goose provider's model config.
pub(crate) fn from_model_config(config: &ModelConfig) -> ModelInfo {
    ModelInfo {
        name: config.model_name.clone(),
        context_limit: config.context_limit(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_text_message() {
        let coop_msg = Message::user().with_text("hello world");
        let goose_msg = to_goose_message(&coop_msg);
        let back = from_goose_message(&goose_msg);

        assert_eq!(back.role, Role::User);
        assert_eq!(back.text(), "hello world");
    }

    #[test]
    fn roundtrip_tool_request() {
        let coop_msg = Message::assistant()
            .with_text("Let me read that file.")
            .with_tool_request(
                "call_1",
                "read_file",
                serde_json::json!({"path": "/tmp/test.txt"}),
            );

        let goose_msg = to_goose_message(&coop_msg);
        let back = from_goose_message(&goose_msg);

        assert_eq!(back.role, Role::Assistant);
        assert_eq!(back.text(), "Let me read that file.");

        let reqs = back.tool_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].name, "read_file");
        assert_eq!(reqs[0].id, "call_1");
    }

    #[test]
    fn roundtrip_tool_result() {
        let coop_msg =
            Message::user().with_tool_result("call_1", "file contents here", false);

        let goose_msg = to_goose_message(&coop_msg);
        let back = from_goose_message(&goose_msg);

        assert_eq!(back.role, Role::User);
        assert!(back.has_tool_results());
    }

    #[test]
    fn tool_def_to_mcp() {
        let def = ToolDef::new(
            "read_file",
            "Read a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        );

        let mcp = to_mcp_tool(&def);
        assert_eq!(mcp.name.as_ref(), "read_file");
        assert!(mcp.description.as_deref() == Some("Read a file"));
    }
}
