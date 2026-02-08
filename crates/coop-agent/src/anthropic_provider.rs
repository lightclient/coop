//! Direct Anthropic API provider with OAuth token support.
//!
//! Uses Bearer auth and Claude Code identity headers for OAuth tokens (sk-ant-oat01-*).
//! OAuth calling convention derived from the opencode-anthropic-auth project.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{Instrument, debug, info, info_span, warn};

use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Content, Message, ModelInfo, Role, ToolDef, Usage};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_VERSION: &str = "2.1.29";

/// Tool name prefix required by Claude Code OAuth calling convention.
const TOOL_PREFIX: &str = "mcp_";

/// Max retry attempts for transient errors.
const MAX_RETRIES: u32 = 3;

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
    /// Automatically detects OAuth tokens (sk-ant-oat*) vs regular API keys.
    pub fn new(api_key: String, model_name: &str) -> Result<Self> {
        let is_oauth = api_key.contains("sk-ant-oat");

        debug!(
            model = model_name,
            is_oauth = is_oauth,
            "creating anthropic provider"
        );

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("failed to create HTTP client")?;

        // Strip provider prefix (e.g. "anthropic/claude-sonnet-4-20250514" -> "claude-sonnet-4-20250514")
        let api_model = model_name.strip_prefix("anthropic/").unwrap_or(model_name);

        let model = ModelInfo {
            name: api_model.to_owned(),
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
    fn build_request(&self, body: &Value, has_tools: bool) -> reqwest::RequestBuilder {
        let url = if self.is_oauth {
            // OAuth requires ?beta=true query parameter
            format!("{ANTHROPIC_API_URL}?beta=true")
        } else {
            ANTHROPIC_API_URL.to_owned()
        };

        let mut req = self
            .client
            .post(&url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");

        if self.is_oauth {
            // Build beta flags: always need oauth + interleaved-thinking,
            // add claude-code flag only when tools are present
            let beta = if has_tools {
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
            } else {
                "oauth-2025-04-20,interleaved-thinking-2025-05-14"
            };

            req = req
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("anthropic-beta", beta)
                .header(
                    "user-agent",
                    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)"),
                )
                .header("x-app", "cli");
        } else {
            req = req.header("x-api-key", &self.api_key);
        }

        req.json(body)
    }

    /// Send a request with retry on transient errors (429, 500, 502, 503).
    async fn send_with_retry(&self, body: &Value, has_tools: bool) -> Result<reqwest::Response> {
        let mut last_err = None;

        for attempt in 0..=MAX_RETRIES {
            let response = self
                .build_request(body, has_tools)
                .send()
                .await
                .context("failed to send request to Anthropic")?;

            let status = response.status();
            debug!(status = %status, attempt = attempt + 1, "http response");

            if status.is_success() {
                return Ok(response);
            }

            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503);

            if !is_retryable || attempt == MAX_RETRIES {
                let error_text = response.text().await.unwrap_or_default();
                anyhow::bail!("Anthropic API error: {status} - {error_text}");
            }

            let error_text = response.text().await.unwrap_or_default();
            let base_ms = 1000u64 * 2u64.pow(attempt);
            // Simple jitter without rand crate
            let jitter_ms = u64::from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_nanos()
                    % 500,
            );
            let backoff_ms = base_ms + jitter_ms;
            warn!(
                attempt = attempt + 1,
                max = MAX_RETRIES,
                status = %status,
                backoff_ms,
                "retryable Anthropic error, backing off: {error_text}"
            );

            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;

            last_err = Some(format!("{status} - {error_text}"));
        }

        anyhow::bail!(
            "Anthropic API error after retries: {}",
            last_err.unwrap_or_default()
        );
    }

    /// Build system prompt array with Claude Code identity for OAuth tokens.
    fn build_system_blocks(&self, system: &str) -> Value {
        if self.is_oauth {
            // OAuth tokens MUST include Claude Code identity as first system block
            json!([
                {
                    "type": "text",
                    "text": "You are Claude Code, Anthropic's official CLI for Claude.",
                    "cache_control": { "type": "ephemeral" }
                },
                {
                    "type": "text",
                    "text": system,
                    "cache_control": { "type": "ephemeral" }
                }
            ])
        } else {
            json!(system)
        }
    }

    /// Build the request body shared between complete() and stream().
    fn build_body(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        stream: bool,
    ) -> Value {
        let has_tools = !tools.is_empty();
        let mut body = json!({
            "model": self.model.name,
            "max_tokens": 8192,
            "system": self.build_system_blocks(system),
            "messages": Self::format_messages(messages, self.is_oauth),
        });

        if has_tools {
            body["tools"] = json!(Self::format_tools(tools, self.is_oauth));
        }

        if stream {
            body["stream"] = json!(true);
        }

        body
    }

    /// Convert Coop messages to Anthropic API format.
    fn format_messages(messages: &[Message], prefix_tools: bool) -> Vec<Value> {
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
                        Content::ToolRequest {
                            id,
                            name,
                            arguments,
                        } => {
                            let api_name = if prefix_tools {
                                format!("{TOOL_PREFIX}{name}")
                            } else {
                                name.clone()
                            };
                            Some(json!({
                                "type": "tool_use",
                                "id": id,
                                "name": api_name,
                                "input": arguments
                            }))
                        }
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
    fn format_tools(tools: &[ToolDef], prefix: bool) -> Vec<Value> {
        tools
            .iter()
            .map(|t| {
                let name = if prefix {
                    format!("{TOOL_PREFIX}{}", t.name)
                } else {
                    t.name.clone()
                };
                json!({
                    "name": name,
                    "description": t.description,
                    "input_schema": t.parameters
                })
            })
            .collect()
    }

    /// Parse Anthropic response into Coop message, stripping tool name prefixes.
    fn parse_response(response: &AnthropicResponse, strip_prefix: bool) -> Message {
        let mut msg = Message::assistant();

        for block in &response.content {
            match block {
                ContentBlock::Text { text } => {
                    msg = msg.with_text(text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    let coop_name = if strip_prefix {
                        name.strip_prefix(TOOL_PREFIX).unwrap_or(name).to_owned()
                    } else {
                        name.clone()
                    };
                    msg = msg.with_tool_request(id, coop_name, input.clone());
                }
                ContentBlock::Thinking { .. } => {
                    // Skip thinking blocks from interleaved-thinking beta
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
            stop_reason: response.stop_reason.clone(),
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
    fn name(&self) -> &'static str {
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
        let span = info_span!(
            "anthropic_request",
            model = %self.model.name,
            method = "complete",
            message_count = messages.len(),
            tool_count = tools.len(),
        );

        async {
            let has_tools = !tools.is_empty();
            let body = self.build_body(system, messages, tools, false);

            let response = self.send_with_retry(&body, has_tools).await?;

            let api_response: AnthropicResponse = response
                .json()
                .await
                .context("failed to parse Anthropic response")?;

            let message = Self::parse_response(&api_response, self.is_oauth);
            let usage = Self::parse_usage(&api_response);

            info!(
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                stop_reason = %api_response.stop_reason.as_deref().unwrap_or("unknown"),
                "anthropic response"
            );

            Ok((message, usage))
        }
        .instrument(span)
        .await
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
        let span = info_span!(
            "anthropic_request",
            model = %self.model.name,
            method = "stream",
            message_count = messages.len(),
            tool_count = tools.len(),
        );
        let _enter = span.enter();

        let has_tools = !tools.is_empty();
        let body = self.build_body(system, messages, tools, true);

        let response = self.send_with_retry(&body, has_tools).await?;

        let byte_stream = response.bytes_stream();
        let is_oauth = self.is_oauth;

        let stream = futures::stream::unfold(
            SseState::new(byte_stream, is_oauth),
            |mut state| async move {
                loop {
                    let line = match state.next_line().await {
                        Ok(Some(line)) => line,
                        Ok(None) => return None,
                        Err(e) => return Some((Err(e), state)),
                    };

                    // SSE protocol: lines starting with "data: "
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };

                    if data == "[DONE]" {
                        return None;
                    }

                    let event: SseEvent = match serde_json::from_str(data) {
                        Ok(ev) => ev,
                        Err(e) => {
                            debug!(error = %e, data = data, "skipping unparseable SSE event");
                            continue;
                        }
                    };

                    match state.handle_event(event) {
                        SseAction::YieldDelta(text) => {
                            let msg = Message::assistant().with_text(&text);
                            return Some((Ok((Some(msg), None)), state));
                        }
                        SseAction::YieldFinal(msg, usage) => {
                            return Some((Ok((Some(msg), Some(usage))), state));
                        }
                        SseAction::Continue => {}
                        SseAction::Error(e) => return Some((Err(e), state)),
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    async fn complete_fast(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.complete(system, messages, tools).await
    }
}

// --- SSE streaming state machine ---

/// Tracks an in-progress content block during SSE streaming.
enum BlockAccumulator {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json_buf: String,
    },
    Thinking,
}

/// What to do after processing an SSE event.
enum SseAction {
    Continue,
    YieldDelta(String),
    YieldFinal(Message, Usage),
    Error(anyhow::Error),
}

/// State for the SSE unfold stream.
struct SseState<S> {
    byte_stream: S,
    line_buf: String,
    blocks: Vec<BlockAccumulator>,
    usage: Usage,
    is_oauth: bool,
}

impl<S> SseState<S>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    fn new(byte_stream: S, is_oauth: bool) -> Self {
        Self {
            byte_stream,
            line_buf: String::new(),
            blocks: Vec::new(),
            usage: Usage::default(),
            is_oauth,
        }
    }

    /// Read the next complete SSE line from the byte stream.
    async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(pos) = self.line_buf.find('\n') {
                let line = self.line_buf[..pos].trim_end_matches('\r').to_owned();
                self.line_buf = self.line_buf[pos + 1..].to_string();
                if line.is_empty() {
                    continue;
                }
                return Ok(Some(line));
            }

            match self.byte_stream.next().await {
                Some(Ok(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk);
                    self.line_buf.push_str(&text);
                }
                Some(Err(e)) => return Err(anyhow::anyhow!("SSE stream error: {e}")),
                None => {
                    if self.line_buf.is_empty() {
                        return Ok(None);
                    }
                    let remaining = std::mem::take(&mut self.line_buf);
                    let trimmed = remaining.trim().to_owned();
                    if trimmed.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(trimmed));
                }
            }
        }
    }

    fn handle_event(&mut self, event: SseEvent) -> SseAction {
        match event {
            SseEvent::MessageStart { message } => {
                if let Some(u) = message.usage {
                    self.usage.input_tokens = Some(u.input_tokens);
                    if let Some(out) = u.output_tokens {
                        self.usage.output_tokens = Some(out);
                    }
                }
                SseAction::Continue
            }
            SseEvent::ContentBlockStart { content_block, .. } => {
                match content_block {
                    SseContentBlock::Text { .. } => {
                        self.blocks.push(BlockAccumulator::Text(String::new()));
                    }
                    SseContentBlock::ToolUse { id, name } => {
                        self.blocks.push(BlockAccumulator::ToolUse {
                            id,
                            name,
                            json_buf: String::new(),
                        });
                    }
                    SseContentBlock::Thinking => {
                        self.blocks.push(BlockAccumulator::Thinking);
                    }
                }
                SseAction::Continue
            }
            SseEvent::ContentBlockDelta { delta, .. } => match delta {
                SseDelta::Text { text } => {
                    if let Some(BlockAccumulator::Text(buf)) = self.blocks.last_mut() {
                        buf.push_str(&text);
                    }
                    SseAction::YieldDelta(text)
                }
                SseDelta::InputJson { partial_json } => {
                    if let Some(BlockAccumulator::ToolUse { json_buf, .. }) = self.blocks.last_mut()
                    {
                        json_buf.push_str(&partial_json);
                    }
                    SseAction::Continue
                }
                SseDelta::Thinking { .. } | SseDelta::Signature { .. } => SseAction::Continue,
            },
            SseEvent::ContentBlockStop { .. } | SseEvent::Ping => SseAction::Continue,
            SseEvent::MessageDelta { delta, usage } => {
                if let Some(out) = usage.output_tokens {
                    self.usage.output_tokens = Some(out);
                }
                if delta.stop_reason.is_some() {
                    self.usage.stop_reason = delta.stop_reason;
                }
                SseAction::Continue
            }
            SseEvent::MessageStop => {
                let is_oauth = self.is_oauth;
                let blocks: Vec<_> = self.blocks.drain(..).collect();
                let mut msg = Message::assistant();
                for block in blocks {
                    match block {
                        BlockAccumulator::Text(text) => {
                            if !text.is_empty() {
                                msg = msg.with_text(text);
                            }
                        }
                        BlockAccumulator::ToolUse { id, name, json_buf } => {
                            let coop_name = if is_oauth {
                                name.strip_prefix(TOOL_PREFIX).unwrap_or(&name).to_owned()
                            } else {
                                name
                            };
                            let input: Value = serde_json::from_str(&json_buf).unwrap_or_default();
                            msg = msg.with_tool_request(id, coop_name, input);
                        }
                        BlockAccumulator::Thinking => {}
                    }
                }
                SseAction::YieldFinal(msg, self.usage.clone())
            }
            SseEvent::Error { error } => SseAction::Error(anyhow::anyhow!(
                "Anthropic SSE error: {} - {}",
                error.error_type,
                error.message
            )),
        }
    }
}

// --- Anthropic API response types (non-streaming) ---

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
    #[serde(rename = "thinking")]
    Thinking {
        #[allow(dead_code)]
        thinking: String,
    },
}

#[derive(Debug, Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// --- SSE event types ---

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SseEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: SseMessageStart },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        #[allow(dead_code)]
        index: u32,
        content_block: SseContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        #[allow(dead_code)]
        index: u32,
        delta: SseDelta,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        #[allow(dead_code)]
        index: u32,
    },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: SseMessageDeltaDelta,
        usage: SseMessageDeltaUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: SseError },
}

#[derive(Debug, Deserialize)]
struct SseMessageStart {
    usage: Option<SseMessageStartUsage>,
}

#[derive(Debug, Deserialize)]
struct SseMessageStartUsage {
    input_tokens: u32,
    output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SseMessageDeltaDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseMessageDeltaUsage {
    output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SseContentBlock {
    #[serde(rename = "text")]
    Text {
        #[allow(dead_code)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "thinking")]
    Thinking,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SseDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking {
        #[allow(dead_code)]
        thinking: String,
    },
    #[serde(rename = "signature_delta")]
    Signature {
        #[allow(dead_code)]
        signature: String,
    },
}

#[derive(Debug, Deserialize)]
struct SseError {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}
