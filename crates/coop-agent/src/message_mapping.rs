use genai::chat::{
    Binary, BinarySource, ChatMessage, ContentPart, MessageContent, Tool, ToolCall, ToolResponse,
};

use coop_core::types::{Content, Message, Role, ToolDef};

use crate::image_prep::prepare_image_for_provider;
use crate::provider_spec::ProviderKind;

pub(crate) fn build_chat_request(
    provider: ProviderKind,
    system: &[String],
    messages: &[Message],
    tools: &[ToolDef],
) -> genai::chat::ChatRequest {
    let mut chat_request = genai::chat::ChatRequest::default();

    for block in system {
        chat_request = chat_request.append_message(ChatMessage::system(block.clone()));
    }

    for message in messages {
        for mapped in map_message(provider, message) {
            chat_request = chat_request.append_message(mapped);
        }
    }

    if !tools.is_empty() {
        chat_request = chat_request.with_tools(tools.iter().map(map_tool));
    }

    chat_request
}

pub(crate) fn map_tool(tool: &ToolDef) -> Tool {
    Tool::new(tool.name.clone())
        .with_description(tool.description.clone())
        .with_schema(tool.parameters.clone())
}

pub(crate) fn map_message(provider: ProviderKind, message: &Message) -> Vec<ChatMessage> {
    let mut regular_parts = Vec::new();
    let mut tool_responses = Vec::new();
    let include_history_thinking = provider != ProviderKind::OpenAiCompatible;

    for content in &message.content {
        match content {
            Content::Text { text } => regular_parts.push(ContentPart::Text(text.clone())),
            Content::Image { data, mime_type } => {
                if let Some(prepared) = prepare_image_for_provider(provider, data, mime_type) {
                    regular_parts.push(ContentPart::Binary(Binary::from_base64(
                        prepared.mime_type,
                        prepared.data,
                        None,
                    )));
                }
            }
            Content::ToolRequest {
                id,
                name,
                arguments,
            } => regular_parts.push(ContentPart::ToolCall(ToolCall {
                call_id: id.clone(),
                fn_name: name.clone(),
                fn_arguments: arguments.clone(),
                thought_signatures: None,
            })),
            Content::ToolResult {
                id,
                output,
                is_error,
            } => {
                let mut content = output.clone();
                if *is_error {
                    content = format!("ERROR: {content}");
                }
                tool_responses.push(ToolResponse::new(id.clone(), content));
            }
            Content::Thinking {
                thinking,
                signature,
            } => {
                if include_history_thinking {
                    if let Some(signature) = signature {
                        regular_parts.push(ContentPart::ThoughtSignature(signature.clone()));
                    }
                    if !thinking.is_empty() {
                        regular_parts.push(ContentPart::ReasoningContent(thinking.clone()));
                    }
                }
            }
        }
    }

    let mut mapped = Vec::new();

    if !regular_parts.is_empty() {
        let role = match message.role {
            Role::User => ChatRoleOrTool::User,
            Role::Assistant => ChatRoleOrTool::Assistant,
        };
        mapped.push(role.into_message(MessageContent::from_parts(regular_parts)));
    }

    if !tool_responses.is_empty() {
        mapped.extend(tool_responses.into_iter().map(ChatMessage::from));
    }

    mapped
}

pub(crate) fn map_response_message(
    content: &MessageContent,
    reasoning_content: Option<&str>,
) -> Message {
    let mut message = Message::assistant();

    if let Some(reasoning) = reasoning_content.filter(|value| !value.trim().is_empty()) {
        message.content.push(Content::Thinking {
            thinking: reasoning.to_owned(),
            signature: None,
        });
    }

    for part in content.parts() {
        match part {
            ContentPart::Text(text) => message.content.push(Content::Text { text: text.clone() }),
            ContentPart::ThoughtSignature(signature) => message.content.push(Content::Thinking {
                thinking: String::new(),
                signature: Some(signature.clone()),
            }),
            ContentPart::ToolCall(tool_call) => message.content.push(Content::ToolRequest {
                id: tool_call.call_id.clone(),
                name: tool_call.fn_name.clone(),
                arguments: tool_call.fn_arguments.clone(),
            }),
            ContentPart::ReasoningContent(reasoning) => message.content.push(Content::Thinking {
                thinking: reasoning.clone(),
                signature: None,
            }),
            ContentPart::Binary(binary) => {
                if binary.is_image()
                    && let BinarySource::Base64(data) = &binary.source
                {
                    message.content.push(Content::Image {
                        data: data.to_string(),
                        mime_type: binary.content_type.clone(),
                    });
                }
            }
            ContentPart::ToolResponse(_) | ContentPart::Custom(_) => {}
        }
    }

    message
}

enum ChatRoleOrTool {
    User,
    Assistant,
}

impl ChatRoleOrTool {
    fn into_message(self, content: MessageContent) -> ChatMessage {
        match self {
            Self::User => ChatMessage::user(content),
            Self::Assistant => ChatMessage::assistant(content),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_message_splits_tool_results_from_user_content() {
        let message = Message::user()
            .with_text("before")
            .with_tool_result("call_1", "ok", false);

        let mapped = map_message(ProviderKind::OpenAi, &message);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[0].role, genai::chat::ChatRole::User);
        assert_eq!(mapped[1].role, genai::chat::ChatRole::Tool);
    }

    #[test]
    fn map_response_message_preserves_tool_calls() {
        let content = MessageContent::from_parts(vec![ContentPart::ToolCall(ToolCall {
            call_id: "call_1".into(),
            fn_name: "bash".into(),
            fn_arguments: json!({"command": "pwd"}),
            thought_signatures: None,
        })]);

        let message = map_response_message(&content, None);
        let requests = message.tool_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].name, "bash");
    }

    #[test]
    fn map_tool_copies_schema() {
        let tool = ToolDef::new("read_file", "Read a file", json!({"type": "object"}));
        let mapped = map_tool(&tool);
        assert_eq!(mapped.name.to_string(), "read_file");
        assert!(mapped.schema.is_some());
    }

    #[test]
    fn map_response_message_preserves_thought_signatures() {
        let content = MessageContent::from_parts(vec![
            ContentPart::ThoughtSignature("sig_1".into()),
            ContentPart::ToolCall(ToolCall {
                call_id: "call_1".into(),
                fn_name: "bash".into(),
                fn_arguments: json!({"command": "pwd"}),
                thought_signatures: Some(vec!["sig_1".into()]),
            }),
        ]);

        let message = map_response_message(&content, None);
        assert!(matches!(
            message.content.first(),
            Some(Content::Thinking {
                signature: Some(signature),
                ..
            }) if signature == "sig_1"
        ));
        assert_eq!(message.tool_requests()[0].name, "bash");
    }

    #[test]
    fn map_response_message_preserves_base64_images() {
        let content = MessageContent::from_parts(vec![ContentPart::Binary(Binary::from_base64(
            "image/png",
            "YWJj",
            None,
        ))]);

        let message = map_response_message(&content, None);
        assert!(matches!(
            message.content.first(),
            Some(Content::Image { mime_type, data }) if mime_type == "image/png" && data == "YWJj"
        ));
    }

    #[test]
    fn map_message_emits_thought_signatures() {
        let message = Message::assistant()
            .with_content(Content::Thinking {
                thinking: String::new(),
                signature: Some("sig_1".into()),
            })
            .with_tool_request("call_1", "bash", json!({"command": "pwd"}));

        let mapped = map_message(ProviderKind::Gemini, &message);
        let parts = mapped[0].content.parts();
        assert!(
            matches!(parts[0], ContentPart::ThoughtSignature(ref signature) if signature == "sig_1")
        );
        assert!(matches!(parts[1], ContentPart::ToolCall(_)));
    }

    #[test]
    fn map_message_skips_thinking_for_openai_compatible() {
        let message = Message::assistant()
            .with_content(Content::Thinking {
                thinking: "internal reasoning".into(),
                signature: Some("sig_1".into()),
            })
            .with_text("final answer")
            .with_tool_request("call_1", "bash", json!({"command": "pwd"}));

        let mapped = map_message(ProviderKind::OpenAiCompatible, &message);
        let parts = mapped[0].content.parts();
        assert!(matches!(parts[0], ContentPart::Text(ref text) if text == "final answer"));
        assert!(matches!(parts[1], ContentPart::ToolCall(_)));
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn map_message_preserves_thinking_for_openai() {
        let message = Message::assistant()
            .with_content(Content::Thinking {
                thinking: "internal reasoning".into(),
                signature: Some("sig_1".into()),
            })
            .with_text("final answer");

        let mapped = map_message(ProviderKind::OpenAi, &message);
        let parts = mapped[0].content.parts();
        assert!(matches!(
            parts[0],
            ContentPart::ThoughtSignature(ref signature) if signature == "sig_1"
        ));
        assert!(matches!(
            parts[1],
            ContentPart::ReasoningContent(ref reasoning) if reasoning == "internal reasoning"
        ));
        assert!(matches!(parts[2], ContentPart::Text(ref text) if text == "final answer"));
    }
}
