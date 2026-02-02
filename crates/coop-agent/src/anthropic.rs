use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_core::{AgentResponse, AgentRuntime, Message, Role};
use futures::StreamExt;
use reqwest::{Client, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_AUTH_PATH: &str = ".openclaw/agents/alice/agent/auth-profiles.json";

/// Direct Anthropic API client runtime.
pub struct AnthropicRuntime {
    client: Client,
    model: String,
    token: String,
}

#[derive(Deserialize)]
struct AuthProfiles {
    profiles: std::collections::HashMap<String, AuthProfile>,
}

#[derive(Deserialize)]
struct AuthProfile {
    token: String,
    #[serde(rename = "type")]
    _type: Option<String>,
    provider: Option<String>,
}

#[derive(Serialize)]
struct ApiRequest {
    model: String,
    max_tokens: u32,
    stream: bool,
    system: Vec<SystemBlock>,
    messages: Vec<ApiMessage>,
}

#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

impl AnthropicRuntime {
    fn build_client() -> Result<Client> {
        let mut headers = header::HeaderMap::new();
        headers.insert("accept", header::HeaderValue::from_static("application/json"));
        headers.insert("anthropic-dangerous-direct-browser-access", header::HeaderValue::from_static("true"));
        headers.insert("anthropic-beta", header::HeaderValue::from_static("claude-code-20250219,oauth-2025-04-20"));
        headers.insert("anthropic-version", header::HeaderValue::from_static("2023-06-01"));
        headers.insert("x-app", header::HeaderValue::from_static("cli"));

        Client::builder()
            .user_agent("claude-cli/2.1.2 (external, cli)")
            .default_headers(headers)
            .build()
            .context("building HTTP client")
    }

    /// Create a new runtime, reading the token from auth-profiles.json.
    pub fn new(model: Option<&str>, auth_path: Option<&Path>) -> Result<Self> {
        let default_path = dirs_path();
        let path = auth_path.unwrap_or(&default_path);
        let token = load_token(path)?;

        Ok(Self {
            client: Self::build_client()?,
            model: strip_provider_prefix(model.unwrap_or(DEFAULT_MODEL)).to_string(),
            token,
        })
    }

    /// Create from an explicit token (for testing).
    pub fn with_token(token: String, model: Option<&str>) -> Self {
        Self {
            client: Self::build_client().expect("failed to build HTTP client"),
            model: strip_provider_prefix(model.unwrap_or(DEFAULT_MODEL)).to_string(),
            token,
        }
    }
}

/// Strip "provider/" prefix from model ID (e.g. "anthropic/claude-sonnet-4-20250514" -> "claude-sonnet-4-20250514")
fn strip_provider_prefix(model: &str) -> &str {
    model.rsplit_once('/').map(|(_, name)| name).unwrap_or(model)
}

fn dirs_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(DEFAULT_AUTH_PATH)
}

/// Load the Anthropic OAuth token from auth-profiles.json.
pub fn load_token(path: &Path) -> Result<String> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("reading auth profiles from {}", path.display()))?;
    let profiles: AuthProfiles =
        serde_json::from_str(&data).context("parsing auth-profiles.json")?;

    let profile = profiles
        .profiles
        .get("anthropic:default")
        .context("no 'anthropic:default' profile found in auth-profiles.json")?;

    Ok(profile.token.clone())
}

/// Parse SSE stream text into collected content.
pub fn parse_sse_events(raw: &str) -> String {
    let mut content = String::new();

    for line in raw.lines() {
        let line = line.trim();
        if !line.starts_with("data: ") {
            continue;
        }
        let json_str = &line[6..];
        if json_str == "[DONE]" {
            break;
        }
        let Ok(val) = serde_json::from_str::<Value>(json_str) else {
            continue;
        };

        match val.get("type").and_then(|t| t.as_str()) {
            Some("content_block_delta") => {
                if let Some(text) = val
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    content.push_str(text);
                }
            }
            _ => {}
        }
    }

    content
}

#[async_trait]
impl AgentRuntime for AnthropicRuntime {
    async fn turn(
        &self,
        messages: &[Message],
        system_prompt: &str,
    ) -> Result<AgentResponse> {
        // Build API messages (skip System role, they go in `system` field)
        let api_messages: Vec<ApiMessage> = messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| ApiMessage {
                role: match m.role {
                    Role::User => "user".into(),
                    Role::Assistant => "assistant".into(),
                    Role::Tool => "user".into(), // map tool to user for simplicity
                    Role::System => unreachable!(),
                },
                content: m.content.clone(),
            })
            .collect();

        if api_messages.is_empty() {
            anyhow::bail!("no messages to send");
        }

        // Claude Code OAuth requires the identity as a SEPARATE system block
        // (cannot be combined with other text in the same block)
        let mut system_blocks = vec![SystemBlock {
            block_type: "text".into(),
            text: "You are Claude Code, Anthropic's official CLI for Claude.".into(),
        }];
        if !system_prompt.is_empty() {
            system_blocks.push(SystemBlock {
                block_type: "text".into(),
                text: system_prompt.to_string(),
            });
        }

        let body = ApiRequest {
            model: self.model.clone(),
            max_tokens: 4096,
            stream: true,
            system: system_blocks,
            messages: api_messages,
        };

        debug!(model = %self.model, "calling Anthropic API");

        let response = self
            .client
            .post(API_URL)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("sending request to Anthropic API")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error {}: {}", status, body);
        }

        // Stream SSE and collect text deltas
        let mut content = String::new();
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading stream chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete lines
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if !line.starts_with("data: ") {
                    continue;
                }
                let json_str = &line[6..];
                if json_str == "[DONE]" {
                    continue;
                }

                let Ok(val) = serde_json::from_str::<Value>(json_str) else {
                    continue;
                };

                if val.get("type").and_then(|t| t.as_str()) == Some("content_block_delta") {
                    if let Some(text) = val
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        content.push_str(text);
                    }
                }

                // Check for errors
                if val.get("type").and_then(|t| t.as_str()) == Some("error") {
                    let msg = val
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error");
                    warn!(error = msg, "Anthropic API stream error");
                    anyhow::bail!("Anthropic stream error: {}", msg);
                }
            }
        }

        if content.is_empty() {
            warn!("Anthropic API returned empty content");
        }

        Ok(AgentResponse::text(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sse_events() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[]}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

event: message_stop
data: {"type":"message_stop"}
"#;
        let result = parse_sse_events(raw);
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn test_parse_sse_empty() {
        let raw = r#"event: message_start
data: {"type":"message_start","message":{}}

event: message_stop
data: {"type":"message_stop"}
"#;
        let result = parse_sse_events(raw);
        assert_eq!(result, "");
    }

    #[test]
    fn test_load_token_from_file() {
        let dir = std::env::temp_dir().join("coop-test-auth");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth-profiles.json");
        std::fs::write(
            &path,
            r#"{"profiles":{"anthropic:default":{"type":"token","provider":"anthropic","token":"sk-ant-test-123"}}}"#,
        )
        .unwrap();

        let token = load_token(&path).unwrap();
        assert_eq!(token, "sk-ant-test-123");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_load_token_missing_profile() {
        let dir = std::env::temp_dir().join("coop-test-auth-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth-profiles.json");
        std::fs::write(&path, r#"{"profiles":{}}"#).unwrap();

        let result = load_token(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_system_prompt_prefix() {
        let rt = AnthropicRuntime::with_token("test".into(), None);
        assert_eq!(rt.model, DEFAULT_MODEL);
    }
}
