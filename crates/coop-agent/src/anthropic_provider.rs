//! Direct Anthropic API provider with OAuth token support.
//!
//! Uses Bearer auth instead of x-api-key to support OAuth tokens (sk-ant-oat01-*).

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Content, Message, ModelInfo, Role, ToolDef, Usage};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Direct Anthropic provider with OAuth support.
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: ModelInfo,
    is_oauth: bool,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    ///
    /// Automatically detects OAuth tokens (sk-ant-oat01-*) vs regular API keys.
    pub fn new(api_key: String, model_name: &str) -> Result<Self> {
        let is_oauth = api_key.starts_with("sk-ant-oat01-");

        debug!(
            model = model_name,
            is_oauth = is_oauth,
            "creating anthropic provider"
        );

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("failed to create HTTP client")?;

        let model = ModelInfo {
            name: model_name.to_string(),
            context_limit: 200_000,
        };

        Ok(Self {
            client,
            api_key,
            model,
            is_oauth,
        })
    }

    /// Create from environment variable ANTHROPIC_API_KEY.
    pub fn from_env(model_name: &str) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY environment variable not set")?;
        Self::new(api_key, model_name)
    }

    /// Build request with appropriate auth headers.
    fn build_request(&self, body: Value) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");

        if self.is_oauth {
            // OAuth tokens use Bearer auth
            req = req
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("anthropic-beta", "oauth-2025-04-20");
        } else {
            // Regular API keys use x-api-key
            req = req.header("x-api-key", &self.api_key);
        }

        req.json(&body)
    }

    /// Convert Coop messages to Anthropic API format.
    fn format_messages(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };

                let content: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(json!({
                            "type": "text",
                            "text": text
                        })),
                        Content::ToolRequest { id, name, arguments } => Some(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": arguments
                        })),
                        Content::ToolResult {
                            id,
                            output,
                            is_error,
                        } => Some(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": output,
                            "is_error": is_error
                        })),
                        _ => None, // Skip Image, Thinking for now
                    })
                    .collect();

                json!({
                    "role": role,
                    "content": content
                })
            })
            .collect()
    }

    /// Convert Coop tools to Anthropic API format.
    fn format_tools(tools: &[ToolDef]) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters
                })
            })
            .collect()
    }

    /// Parse Anthropic response into Coop message.
    fn parse_response(response: &AnthropicResponse) -> Message {
        let mut msg = Message::assistant();

        for block in &response.content {
            match block {
                ContentBlock::Text { text } => {
                    msg = msg.with_text(text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    msg = msg.with_tool_request(id, name, input.clone());
                }
            }
        }

        msg
    }

    /// Parse usage from response.
    fn parse_usage(response: &AnthropicResponse) -> Usage {
        Usage {
            input_tokens: Some(response.usage.input_tokens),
            output_tokens: Some(response.usage.output_tokens),
            ..Default::default()
        }
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("model", &self.model.name)
            .field("is_oauth", &self.is_oauth)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model_info(&self) -> &ModelInfo {
        &self.model
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let mut body = json!({
            "model": self.model.name,
            "max_tokens": 8192,
            "system": system,
            "messages": Self::format_messages(messages),
        });

        if !tools.is_empty() {
            body["tools"] = json!(Self::format_tools(tools));
        }

        let response = self
            .build_request(body)
            .send()
            .await
            .context("failed to send request to Anthropic")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Anthropic API error: {} - {}",
                status,
                error_text
            );
        }

        let api_response: AnthropicResponse = response
            .json()
            .await
            .context("failed to parse Anthropic response")?;

        let message = Self::parse_response(&api_response);
        let usage = Self::parse_usage(&api_response);

        Ok((message, usage))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        // For now, fall back to non-streaming
        // TODO: Implement SSE streaming
        let (message, usage) = self.complete(system, messages, tools).await?;
        let stream = futures::stream::once(async move { Ok((Some(message), Some(usage))) });
        Ok(Box::pin(stream))
    }

    async fn complete_fast(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        // Same as complete for now
        self.complete(system, messages, tools).await
    }
}

// --- Anthropic API response types ---

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage: ApiUsage,
    #[allow(dead_code)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Debug, Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}
