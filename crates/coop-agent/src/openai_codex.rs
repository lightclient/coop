use crate::openai_codex_parser::parse_codex_sse;
use crate::provider_spec::{OpenAiReasoningConfig, OpenAiReasoningEffort};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use coop_core::{Content, Message, ToolDef, Usage};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

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
    pub reasoning: Option<&'a OpenAiReasoningConfig>,
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
        request.reasoning,
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
    reasoning: Option<&OpenAiReasoningConfig>,
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

    if let Some(reasoning) = reasoning.and_then(|value| format_codex_reasoning(model, value)) {
        body["reasoning"] = reasoning;
    }

    body
}

fn format_codex_reasoning(model: &str, reasoning: &OpenAiReasoningConfig) -> Option<Value> {
    let effort = reasoning.effort?;
    Some(json!({
        "effort": clamp_codex_reasoning_effort(model, effort),
        "summary": reasoning.summary.map_or("auto", |value| value.as_str()),
    }))
}

fn clamp_codex_reasoning_effort(model: &str, effort: OpenAiReasoningEffort) -> &'static str {
    let model_id = model.rsplit('/').next().unwrap_or(model);
    if (model_id.starts_with("gpt-5.2")
        || model_id.starts_with("gpt-5.3")
        || model_id.starts_with("gpt-5.4"))
        && matches!(effort, OpenAiReasoningEffort::Minimal)
    {
        return OpenAiReasoningEffort::Low.as_str();
    }
    if model_id == "gpt-5.1" && matches!(effort, OpenAiReasoningEffort::Xhigh) {
        return OpenAiReasoningEffort::High.as_str();
    }
    if model_id == "gpt-5.1-codex-mini" {
        return match effort {
            OpenAiReasoningEffort::High | OpenAiReasoningEffort::Xhigh => {
                OpenAiReasoningEffort::High.as_str()
            }
            _ => OpenAiReasoningEffort::Medium.as_str(),
        };
    }
    effort.as_str()
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
    fn build_codex_body_includes_reasoning_config() {
        let body = build_codex_body(
            "gpt-5.4",
            &[],
            &[],
            &[],
            Some(&OpenAiReasoningConfig {
                effort: Some(OpenAiReasoningEffort::Minimal),
                summary: None,
            }),
        );

        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["reasoning"]["summary"], "auto");
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
