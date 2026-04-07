use anyhow::Result;
use coop_core::{Content, Message, Usage};
use serde_json::{Value, json};
use tracing::{debug, warn};

pub(super) fn parse_codex_sse(body: &str) -> Result<(Message, Usage)> {
    let normalized = body.replace("\r\n", "\n");
    let mut message = Message::assistant();
    let mut final_response = None;
    let mut event_count = 0usize;
    let mut output_item_done_count = 0usize;
    let mut terminal_event = None;

    for chunk in normalized.split("\n\n") {
        let data_lines: Vec<&str> = chunk
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("data:"))
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "[DONE]")
            .collect();
        if data_lines.is_empty() {
            continue;
        }

        let data = data_lines.join("\n");
        let event: Value = match serde_json::from_str(&data) {
            Ok(event) => event,
            Err(_) => continue,
        };
        event_count += 1;

        match event.get("type").and_then(Value::as_str) {
            Some("error") => {
                let message = event
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("OpenAI Codex stream error");
                anyhow::bail!("{message}");
            }
            Some("response.failed") => {
                let message = event
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        event
                            .get("response")
                            .and_then(|response| response.get("incomplete_details"))
                            .and_then(|details| details.get("reason"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("OpenAI Codex response failed");
                anyhow::bail!("{message}");
            }
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    append_output_item(&mut message, item);
                    output_item_done_count += 1;
                }
            }
            Some("response.done" | "response.completed" | "response.incomplete") => {
                terminal_event = event.get("type").and_then(Value::as_str).map(str::to_owned);
                final_response = event.get("response").cloned();
                break;
            }
            _ => {}
        }
    }

    let response = final_response
        .ok_or_else(|| anyhow::anyhow!("OpenAI Codex stream ended without a completed response"))?;
    let response_output_message = parse_codex_response_message(&response);
    let used_response_output_fallback =
        should_use_response_output(&message, &response_output_message);
    if used_response_output_fallback {
        message = response_output_message;
    }

    let usage = parse_codex_usage(&response, &message);
    let text_block_count = message
        .content
        .iter()
        .filter(|content| matches!(content, Content::Text { .. }))
        .count();
    let reasoning_block_count = message
        .content
        .iter()
        .filter(|content| matches!(content, Content::Thinking { .. }))
        .count();

    debug!(
        event_count,
        output_item_done_count,
        terminal_event = terminal_event.as_deref().unwrap_or("missing"),
        text_block_count,
        tool_request_count = message.tool_requests().len(),
        reasoning_block_count,
        response_text_len = message.text().len(),
        used_response_output_fallback,
        "parsed OpenAI Codex SSE response"
    );

    Ok((message, usage))
}

fn should_use_response_output(message: &Message, response_output_message: &Message) -> bool {
    if message.content.is_empty() {
        return !response_output_message.content.is_empty();
    }

    message.text().trim().is_empty()
        && !message.has_tool_requests()
        && (!response_output_message.text().trim().is_empty()
            || response_output_message.has_tool_requests())
}

fn append_output_item(message: &mut Message, item: &Value) {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            let text = collect_message_text(item);
            if !text.is_empty() {
                message.content.push(Content::text(text));
            }
        }
        Some("function_call") => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let item_id = item.get("id").and_then(Value::as_str);
            let tool_name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let raw_arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments = parse_tool_arguments(tool_name, raw_arguments);
            let tool_id = item_id.map_or_else(
                || call_id.to_owned(),
                |item_id| format!("{call_id}|{item_id}"),
            );
            message
                .content
                .push(Content::tool_request(tool_id, tool_name, arguments));
        }
        Some("reasoning") => {
            let thinking = item
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n");
            if !thinking.is_empty() {
                message.content.push(Content::Thinking {
                    thinking,
                    signature: Some(item.to_string()),
                });
            }
        }
        _ => {}
    }
}

fn parse_codex_response_message(response: &Value) -> Message {
    let mut message = Message::assistant();

    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        append_output_item(&mut message, item);
    }

    message
}

fn collect_message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn parse_tool_arguments(tool_name: &str, raw_arguments: &str) -> Value {
    serde_json::from_str::<Value>(raw_arguments).unwrap_or_else(|error| {
        warn!(
            tool_name,
            %error,
            raw = raw_arguments,
            "failed to parse OpenAI Codex tool arguments, using empty object"
        );
        json!({})
    })
}

fn parse_codex_usage(response: &Value, message: &Message) -> Usage {
    Usage {
        input_tokens: response
            .get("usage")
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        output_tokens: response
            .get("usage")
            .and_then(|usage| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        cache_read_tokens: response
            .get("usage")
            .and_then(|usage| usage.get("input_tokens_details"))
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        stop_reason: Some(stop_reason(response, message)),
        ..Default::default()
    }
}

fn stop_reason(response: &Value, message: &Message) -> String {
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    if status == "completed" && message.has_tool_requests() {
        return "tool_use".to_owned();
    }

    match status {
        "incomplete" => "length",
        "failed" | "cancelled" => "error",
        "queued" | "in_progress" | "completed" => "stop",
        _ => "stop",
    }
    .to_owned()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_output_item_done_text_response() {
        let body = r#"data: {"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Hello","annotations":[]}]}}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":12,"output_tokens":3}}}

"#;
        let (message, usage) = parse_codex_sse(body).unwrap();
        assert_eq!(message.text(), "Hello");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parses_response_done_terminal_event() {
        let body = r#"data: {"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Done alias","annotations":[]}]}}

data: {"type":"response.done","response":{"status":"completed","usage":{"input_tokens":4,"output_tokens":2}}}

"#;
        let (message, usage) = parse_codex_sse(body).unwrap();
        assert_eq!(message.text(), "Done alias");
        assert_eq!(usage.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn falls_back_to_response_output_when_output_items_missing() {
        let body = r#"event: response.completed
 data: {"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"Fallback OK"}]}],"usage":{"input_tokens":12,"output_tokens":3}}}

"#;
        let (message, usage) = parse_codex_sse(body).unwrap();
        assert_eq!(message.text(), "Fallback OK");
        assert_eq!(usage.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parses_output_item_done_tool_response() {
        let body = r#"data: {"type":"response.output_item.done","item":{"type":"function_call","id":"fc_123","call_id":"call_123","name":"bash","arguments":"{\"command\":\"pwd\"}"}}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":9,"output_tokens":2}}}

"#;
        let (message, usage) = parse_codex_sse(body).unwrap();
        let Some((id, name, arguments)) = message.content[0].as_tool_request() else {
            panic!("expected tool request");
        };
        assert_eq!(id, "call_123|fc_123");
        assert_eq!(name, "bash");
        assert_eq!(arguments["command"], "pwd");
        assert_eq!(usage.stop_reason.as_deref(), Some("tool_use"));
    }
}
