use coop_core::{Content, Message, ToolDef};
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub(super) struct ProviderRequestMetrics {
    pub system_chars: usize,
    pub message_chars: usize,
    pub tool_schema_bytes: usize,
    pub estimated_json_bytes: usize,
}

#[derive(Serialize)]
struct RequestEnvelope<'a> {
    system: &'a [String],
    messages: &'a [Message],
    tools: &'a [ToolDef],
}

pub(super) fn estimate_provider_request_metrics(
    system_prompt: &[String],
    messages: &[Message],
    tool_defs: &[ToolDef],
) -> ProviderRequestMetrics {
    let system_chars = system_prompt.iter().map(String::len).sum();
    let message_chars = messages.iter().map(estimate_message_chars).sum();
    let tool_schema_bytes = serde_json::to_vec(tool_defs).map_or(0, |bytes| bytes.len());
    let estimated_json_bytes = serde_json::to_vec(&RequestEnvelope {
        system: system_prompt,
        messages,
        tools: tool_defs,
    })
    .map_or(system_chars + message_chars + tool_schema_bytes, |bytes| {
        bytes.len()
    });

    ProviderRequestMetrics {
        system_chars,
        message_chars,
        tool_schema_bytes,
        estimated_json_bytes,
    }
}

fn estimate_message_chars(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            Content::Text { text } => text.len(),
            Content::ToolRequest {
                id,
                name,
                arguments,
            } => id.len() + name.len() + arguments.to_string().len(),
            Content::ToolResult { id, output, .. } => id.len() + output.len(),
            Content::Image { data, mime_type } => data.len() + mime_type.len(),
            Content::Thinking {
                thinking,
                signature,
            } => thinking.len() + signature.as_ref().map_or(0, String::len),
        })
        .sum()
}
