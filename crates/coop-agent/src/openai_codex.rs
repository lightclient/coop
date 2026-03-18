use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use coop_core::{Content, Message, ToolDef, Usage};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tracing::warn;

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const OPENAI_BETA_RESPONSES: &str = "responses=experimental";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpenAiAuthMode {
    ApiKey,
    CodexOAuth { account_id: String },
}

impl OpenAiAuthMode {
    pub(super) fn detect(token: &str) -> Self {
        extract_account_id(token).map_or(Self::ApiKey, |account_id| Self::CodexOAuth { account_id })
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::CodexOAuth { .. } => "codex_oauth",
        }
    }
}

pub(super) fn api_model_name(model_name: &str, auth_mode: &OpenAiAuthMode) -> String {
    let api_model = model_name.strip_prefix("openai/").unwrap_or(model_name);
    match auth_mode {
        OpenAiAuthMode::ApiKey => api_model.to_owned(),
        OpenAiAuthMode::CodexOAuth { .. } => codex_model_name(api_model),
    }
}

pub(super) struct CodexRequest<'a> {
    pub model: &'a str,
    pub system: &'a [String],
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
}

pub(super) async fn complete_codex(
    client: &Client,
    access_token: &str,
    account_id: &str,
    request: CodexRequest<'_>,
) -> Result<(Message, Usage)> {
    let body = build_codex_body(
        request.model,
        request.system,
        request.messages,
        request.tools,
    );
    let response = client
        .post(CODEX_RESPONSES_URL)
        .header("authorization", format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header("originator", "coop")
        .header("openai-beta", OPENAI_BETA_RESPONSES)
        .header("accept", "text/event-stream")
        .header("content-type", "application/json")
        .header("user-agent", format!("coop/{}", env!("CARGO_PKG_VERSION")))
        .json(&body)
        .send()
        .await
        .context("failed to send request to OpenAI Codex")?;

    let status = response.status();
    let response_text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{}", format_codex_error(status, &response_text));
    }

    parse_codex_sse(&response_text)
}

fn codex_model_name(model: &str) -> String {
    model
        .strip_prefix("codex-gpt-")
        .map_or_else(|| model.to_owned(), |suffix| format!("gpt-{suffix}"))
}

pub(crate) fn extract_account_id(token: &str) -> Option<String> {
    let claims = decode_jwt_payload(token)?;
    claims
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

/// Extract the `exp` claim from a JWT, returning milliseconds since epoch.
pub(crate) fn jwt_expires_at_ms(token: &str) -> Option<i64> {
    let claims = decode_jwt_payload(token)?;
    claims.get("exp").and_then(Value::as_i64).map(|s| s * 1000)
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn build_codex_body(
    model: &str,
    system: &[String],
    messages: &[Message],
    tools: &[ToolDef],
) -> Value {
    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": system.join("\n\n"),
        "input": build_codex_input(messages),
        "text": { "verbosity": "medium" },
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });

    if !tools.is_empty() {
        body["tools"] = json!(format_codex_tools(tools));
    }

    body
}

fn build_codex_input(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();

    for message in messages {
        match message.role {
            coop_core::Role::User => {
                let mut user_content = Vec::new();
                for block in &message.content {
                    match block {
                        Content::Text { text } => user_content.push(json!({
                            "type": "input_text",
                            "text": text,
                        })),
                        Content::Image { data, mime_type } => user_content.push(json!({
                            "type": "input_image",
                            "detail": "auto",
                            "image_url": format!("data:{mime_type};base64,{data}"),
                        })),
                        Content::ToolResult { id, output, .. } => {
                            if !user_content.is_empty() {
                                items.push(json!({
                                    "role": "user",
                                    "content": std::mem::take(&mut user_content),
                                }));
                            }
                            let (call_id, _) = split_tool_call_id(id);
                            items.push(json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": output,
                            }));
                        }
                        Content::ToolRequest { .. } | Content::Thinking { .. } => {}
                    }
                }
                if !user_content.is_empty() {
                    items.push(json!({
                        "role": "user",
                        "content": user_content,
                    }));
                }
            }
            coop_core::Role::Assistant => {
                for (index, block) in message.content.iter().enumerate() {
                    match block {
                        Content::Text { text } => items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "status": "completed",
                            "id": assistant_item_id(&message.id, index),
                            "content": [{
                                "type": "output_text",
                                "text": text,
                                "annotations": [],
                            }],
                        })),
                        Content::ToolRequest {
                            id,
                            name,
                            arguments,
                        } => {
                            let (call_id, item_id) = split_tool_call_id(id);
                            let mut tool_call = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": serde_json::to_string(arguments)
                                    .unwrap_or_else(|_| "{}".to_owned()),
                            });
                            if let Some(item_id) = item_id {
                                tool_call["id"] = json!(item_id);
                            }
                            items.push(tool_call);
                        }
                        Content::Thinking {
                            signature: Some(signature),
                            ..
                        } => {
                            if let Ok(item) = serde_json::from_str::<Value>(signature) {
                                items.push(item);
                            }
                        }
                        Content::Image { .. }
                        | Content::ToolResult { .. }
                        | Content::Thinking { .. } => {}
                    }
                }
            }
        }
    }

    items
}

fn format_codex_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": false,
            })
        })
        .collect()
}

fn assistant_item_id(message_id: &str, index: usize) -> String {
    let candidate = format!("msg_{message_id}_{index}");
    if candidate.len() <= 64 {
        candidate
    } else {
        format!("msg_{index}")
    }
}

fn split_tool_call_id(id: &str) -> (&str, Option<&str>) {
    if let Some((call_id, item_id)) = id.split_once('|') {
        (call_id, Some(item_id))
    } else {
        (id, None)
    }
}

fn parse_codex_sse(body: &str) -> Result<(Message, Usage)> {
    let normalized = body.replace("\r\n", "\n");
    let mut final_response = None;

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
            Some("response.completed" | "response.incomplete") => {
                final_response = event.get("response").cloned();
                break;
            }
            _ => {}
        }
    }

    let response = final_response
        .ok_or_else(|| anyhow::anyhow!("OpenAI Codex stream ended without a completed response"))?;
    Ok(parse_codex_response(&response))
}

fn parse_codex_response(response: &Value) -> (Message, Usage) {
    let mut message = Message::assistant();

    for item in response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => part.get("text").and_then(Value::as_str),
                        Some("refusal") => part.get("refusal").and_then(Value::as_str),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    message = message.with_text(text);
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
                let arguments =
                    serde_json::from_str::<Value>(raw_arguments).unwrap_or_else(|error| {
                        warn!(
                            tool_name,
                            %error,
                            raw = raw_arguments,
                            "failed to parse OpenAI Codex tool arguments, using empty object"
                        );
                        json!({})
                    });
                let tool_id = item_id.map_or_else(
                    || call_id.to_owned(),
                    |item_id| format!("{call_id}|{item_id}"),
                );
                message = message.with_tool_request(tool_id, tool_name, arguments);
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
                    message = message.with_content(Content::Thinking {
                        thinking,
                        signature: Some(item.to_string()),
                    });
                }
            }
            _ => {}
        }
    }

    let usage = Usage {
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
        stop_reason: Some(stop_reason(response, &message)),
        ..Default::default()
    };

    (message, usage)
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
        _ => "stop",
    }
    .to_owned()
}

fn format_codex_error(status: StatusCode, body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        if let Some(detail) = parsed.get("detail").and_then(Value::as_str) {
            return format!("OpenAI Codex API error {}: {detail}", status.as_u16());
        }
        if let Some(message) = parsed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return format!("OpenAI Codex API error {}: {message}", status.as_u16());
        }
    }

    format!(
        "OpenAI Codex API error {}: {}",
        status.as_u16(),
        body.trim()
    )
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jwt(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                JWT_CLAIM_PATH: {
                    "chatgpt_account_id": account_id,
                }
            }))
            .unwrap(),
        );
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn detects_codex_oauth_token() {
        let auth_mode = OpenAiAuthMode::detect(&fake_jwt("acct_123"));
        assert_eq!(
            auth_mode,
            OpenAiAuthMode::CodexOAuth {
                account_id: "acct_123".to_owned()
            }
        );
    }

    #[test]
    fn codex_auth_maps_legacy_codex_model_alias() {
        let auth_mode = OpenAiAuthMode::CodexOAuth {
            account_id: "acct_123".to_owned(),
        };
        assert_eq!(
            api_model_name("openai/codex-gpt-5.4", &auth_mode),
            "gpt-5.4"
        );
    }

    #[test]
    fn parses_completed_codex_text_response() {
        let body = r#"event: response.completed
 data: {"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}],"usage":{"input_tokens":12,"output_tokens":3}}}

"#;
        let (message, usage) = parse_codex_sse(body).unwrap();
        assert_eq!(message.text(), "OK");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.stop_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parses_completed_codex_tool_response() {
        let body = r#"data: {"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"fc_123","call_id":"call_123","name":"bash","arguments":"{\"command\":\"pwd\"}"}],"usage":{"input_tokens":9,"output_tokens":2}}}

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
