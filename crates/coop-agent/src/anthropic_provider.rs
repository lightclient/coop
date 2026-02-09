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
use std::sync::RwLock;
use tracing::{Instrument, debug, info, info_span, warn};

use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Content, Message, ModelInfo, Role, ToolDef, Usage};

use crate::key_pool::KeyPool;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_VERSION: &str = "2.1.29";

/// Tool name prefix required by Claude Code OAuth calling convention.
const TOOL_PREFIX: &str = "mcp_";

/// Max retry attempts for transient errors.
const MAX_RETRIES: u32 = 3;

/// Parse an Anthropic API error response body into a friendly message.
///
/// Anthropic errors are JSON: `{"type":"error","error":{"type":"rate_limit_error","message":"..."}}`
/// This extracts the human-readable message and error type instead of dumping raw JSON.
fn format_api_error(status: reqwest::StatusCode, raw_body: &str) -> String {
    #[derive(Deserialize)]
    struct ApiErrorResponse {
        error: ApiErrorDetail,
    }

    #[derive(Deserialize)]
    struct ApiErrorDetail {
        r#type: String,
        message: String,
    }

    if let Ok(parsed) = serde_json::from_str::<ApiErrorResponse>(raw_body) {
        let label = match parsed.error.r#type.as_str() {
            "rate_limit_error" => "Rate limited",
            "overloaded_error" => "API overloaded",
            "authentication_error" => "Authentication failed",
            "invalid_request_error" => "Invalid request",
            "not_found_error" => "Not found",
            "permission_error" => "Permission denied",
            other => other,
        };
        format!("{label} ({status}): {}", parsed.error.message)
    } else {
        format!("Anthropic API error ({status}): {raw_body}")
    }
}

/// Direct Anthropic provider with OAuth support and key rotation.
pub struct AnthropicProvider {
    client: Client,
    keys: KeyPool,
    model: RwLock<ModelInfo>,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with multiple API keys.
    ///
    /// Each key auto-detects OAuth (sk-ant-oat*) vs regular API keys.
    pub fn new(api_keys: Vec<String>, model_name: &str) -> Result<Self> {
        anyhow::ensure!(!api_keys.is_empty(), "at least one API key is required");

        let key_count = api_keys.len();
        debug!(model = model_name, key_count, "creating anthropic provider");

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
            keys: KeyPool::new(api_keys),
            model: RwLock::new(model),
        })
    }

    /// Create from environment variable ANTHROPIC_API_KEY (single-key, backward compat).
    pub fn from_env(model_name: &str) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY environment variable not set")?;
        Self::new(vec![api_key], model_name)
    }

    /// Create from `env:VAR_NAME` key references.
    pub fn from_key_refs(key_refs: &[String], model_name: &str) -> Result<Self> {
        let keys = crate::key_pool::resolve_key_refs(key_refs)?;
        Self::new(keys, model_name)
    }

    /// Read a snapshot of the current model info.
    fn model_snapshot(&self) -> ModelInfo {
        self.model.read().expect("model lock poisoned").clone()
    }

    /// Build request with appropriate auth headers for a specific key.
    fn build_request(
        &self,
        body: &Value,
        has_tools: bool,
        api_key: &str,
        is_oauth: bool,
    ) -> reqwest::RequestBuilder {
        let url = if is_oauth {
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

        if is_oauth {
            // Build beta flags: always need oauth + interleaved-thinking,
            // add claude-code flag only when tools are present
            let beta = if has_tools {
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"
            } else {
                "oauth-2025-04-20,interleaved-thinking-2025-05-14"
            };

            req = req
                .header("authorization", format!("Bearer {api_key}"))
                .header("anthropic-beta", beta)
                .header(
                    "user-agent",
                    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)"),
                )
                .header("x-app", "cli");
        } else {
            req = req.header("x-api-key", api_key);
        }

        req.json(body)
    }

    /// Send a request with retry on transient errors, rotating keys on 429 rate limits.
    async fn send_with_retry(
        &self,
        body: &Value,
        has_tools: bool,
    ) -> Result<(reqwest::Response, usize)> {
        let mut last_err = None;
        let key_count = self.keys.len();

        for attempt in 0..=MAX_RETRIES {
            let key_index = self.keys.best_key();
            let (api_key, is_oauth) = self.keys.get(key_index);

            let response = self
                .build_request(body, has_tools, api_key, is_oauth)
                .send()
                .await
                .context("failed to send request to Anthropic")?;

            let status = response.status();
            debug!(status = %status, attempt = attempt + 1, key_index, key_count, "http response");

            // Update rate-limit state from headers on every response.
            self.keys.update_from_headers(key_index, response.headers());

            if status.is_success() {
                if self.keys.is_near_limit(key_index) {
                    let utilization = self.keys.utilization(key_index);
                    info!(
                        key_index,
                        key_count,
                        utilization = utilization,
                        "key approaching rate limit, will rotate on next request"
                    );
                }
                return Ok((response, key_index));
            }

            let is_retryable = matches!(status.as_u16(), 429 | 500 | 502 | 503);

            if !is_retryable || attempt == MAX_RETRIES {
                let error_text = response.text().await.unwrap_or_default();
                anyhow::bail!("{}", format_api_error(status, &error_text));
            }

            // Save retry-after before consuming the response body.
            let retry_after_val = parse_retry_after(response.headers());
            let error_text = response.text().await.unwrap_or_default();

            // Check if this is a rate_limit_error (not overloaded).
            let is_rate_limit = status.as_u16() == 429 && error_text.contains("rate_limit_error");

            if is_rate_limit {
                let retry_after = retry_after_val.unwrap_or(60);
                self.keys.mark_rate_limited(key_index, retry_after);

                let next_key = self.keys.best_key();
                if next_key != key_index && !self.keys.on_cooldown(next_key) {
                    info!(
                        old_key = key_index,
                        new_key = next_key,
                        key_count,
                        "rate-limited, rotated key"
                    );
                    continue; // retry immediately with the new key
                }

                warn!(
                    key_index,
                    key_count, retry_after, "all keys rate-limited, waiting"
                );
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            // Overloaded (429 non-rate-limit) or 5xx: exponential backoff
            let base_ms = 1000u64 * 2u64.pow(attempt);
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
                key_index,
                "retryable Anthropic error, backing off: {error_text}"
            );

            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;

            last_err = Some(format_api_error(status, &error_text));
        }

        anyhow::bail!(
            "Anthropic API error after {} retries: {}",
            MAX_RETRIES,
            last_err.unwrap_or_default()
        );
    }

    /// Build system prompt array with cache_control breakpoints.
    ///
    /// OAuth tokens include Claude Code identity as first block.
    /// Both paths use structured arrays for prompt caching.
    fn build_system_blocks(system: &str, is_oauth: bool) -> Value {
        if is_oauth {
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
            json!([{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }])
        }
    }

    /// Build the request body shared between complete() and stream().
    fn build_body(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
        stream: bool,
        is_oauth: bool,
    ) -> Value {
        let has_tools = !tools.is_empty();
        let model = self.model_snapshot();

        // Place cache breakpoint on second-to-last message so the
        // entire conversation prefix is cached — only the newest
        // message (latest content) is uncached.
        let cache_at = (messages.len() >= 2).then(|| messages.len() - 2);

        let mut body = json!({
            "model": model.name,
            "max_tokens": 8192,
            "system": Self::build_system_blocks(system, is_oauth),
            "messages": Self::format_messages(messages, is_oauth, cache_at),
        });

        if has_tools {
            body["tools"] = json!(Self::format_tools(tools, is_oauth));
        }

        if stream {
            body["stream"] = json!(true);
        }

        body
    }

    /// Convert Coop messages to Anthropic API format.
    ///
    /// When `cache_at` is `Some(i)`, the last content block of the
    /// i-th source message gets a `cache_control` breakpoint so the
    /// entire conversation prefix up to that point is cached.
    fn format_messages(
        messages: &[Message],
        prefix_tools: bool,
        cache_at: Option<usize>,
    ) -> Vec<Value> {
        let mut formatted: Vec<Value> = Vec::new();
        let mut source_index = 0;

        for m in messages {
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
                        // Ensure input is a valid JSON object — the API rejects null or string
                        let input = match arguments {
                            Value::Null => json!({}),
                            Value::String(s) => {
                                warn!(
                                    tool_id = id,
                                    tool_name = name,
                                    serialized_args = s,
                                    "tool arguments were incorrectly serialized as string, attempting to parse"
                                );
                                // Try to parse string as JSON, fall back to empty object
                                serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({}))
                            }
                            other => other.clone()
                        };
                        Some(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": api_name,
                            "input": input
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

            // Drop messages with empty content after filtering — Anthropic
            // rejects non-final messages with `"content": []` (BUG-001).
            if content.is_empty() {
                source_index += 1;
                continue;
            }

            let mut msg = json!({
                "role": role,
                "content": content
            });

            if cache_at == Some(source_index)
                && let Some(arr) = msg["content"].as_array_mut()
                && let Some(last_block) = arr.last_mut()
            {
                last_block["cache_control"] = json!({ "type": "ephemeral" });
            }

            formatted.push(msg);
            source_index += 1;
        }

        formatted
    }

    /// Convert Coop tools to Anthropic API format.
    ///
    /// Sets `cache_control` on the last tool definition so that
    /// system prompt + all tools form a cached prefix.
    fn format_tools(tools: &[ToolDef], prefix: bool) -> Vec<Value> {
        let len = tools.len();
        tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let name = if prefix {
                    format!("{TOOL_PREFIX}{}", t.name)
                } else {
                    t.name.clone()
                };
                let mut tool = json!({
                    "name": name,
                    "description": t.description,
                    "input_schema": t.parameters
                });
                if i == len - 1 {
                    tool["cache_control"] = json!({ "type": "ephemeral" });
                }
                tool
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
            cache_read_tokens: response.usage.cache_read_input_tokens,
            cache_write_tokens: response.usage.cache_creation_input_tokens,
            stop_reason: response.stop_reason.clone(),
        }
    }
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model = self.model_snapshot();
        f.debug_struct("AnthropicProvider")
            .field("model", &model.name)
            .field("key_count", &self.keys.len())
            .finish_non_exhaustive()
    }
}

/// Parse `retry-after` header value (seconds).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model_info(&self) -> ModelInfo {
        self.model_snapshot()
    }

    fn set_model(&self, model: &str) {
        let api_model = model.strip_prefix("anthropic/").unwrap_or(model);
        let mut info = self.model.write().expect("model lock poisoned");
        if info.name != api_model {
            debug!(old = %info.name, new = %api_model, "provider model updated");
            api_model.clone_into(&mut info.name);
        }
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let model_name = self.model_snapshot().name;
        let key_count = self.keys.len();
        let span = info_span!(
            "anthropic_request",
            model = %model_name,
            method = "complete",
            message_count = messages.len(),
            tool_count = tools.len(),
            key_count,
        );

        async {
            let has_tools = !tools.is_empty();
            // Use the best key's OAuth status for body building.
            let best = self.keys.best_key();
            let (_, is_oauth) = self.keys.get(best);
            let body = self.build_body(system, messages, tools, false, is_oauth);

            let (response, _key_index) = self.send_with_retry(&body, has_tools).await?;

            let api_response: AnthropicResponse = response
                .json()
                .await
                .context("failed to parse Anthropic response")?;

            let message = Self::parse_response(&api_response, is_oauth);
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
        let model_name = self.model_snapshot().name;
        let key_count = self.keys.len();
        let span = info_span!(
            "anthropic_request",
            model = %model_name,
            method = "stream",
            message_count = messages.len(),
            tool_count = tools.len(),
            key_count,
        );
        let _enter = span.enter();

        let has_tools = !tools.is_empty();
        let best = self.keys.best_key();
        let (_, is_oauth) = self.keys.get(best);
        let body = self.build_body(system, messages, tools, true, is_oauth);

        let (response, _key_index) = self.send_with_retry(&body, has_tools).await?;

        let byte_stream = response.bytes_stream();

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
                    self.usage.cache_read_tokens = u.cache_read_input_tokens;
                    self.usage.cache_write_tokens = u.cache_creation_input_tokens;
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
                if let Some(v) = usage.cache_creation_input_tokens {
                    self.usage.cache_write_tokens = Some(v);
                }
                if let Some(v) = usage.cache_read_input_tokens {
                    self.usage.cache_read_tokens = Some(v);
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
#[allow(clippy::struct_field_names)] // field names match Anthropic API
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
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
#[allow(clippy::struct_field_names)] // field names match Anthropic API
struct SseMessageStartUsage {
    input_tokens: u32,
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SseMessageDeltaDelta {
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)] // field names match Anthropic API
struct SseMessageDeltaUsage {
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// BUG-001: thinking-only assistant messages produced empty content arrays
    /// that Anthropic rejected with 400. format_messages must drop them.
    #[test]
    fn format_messages_drops_thinking_only_assistant_message() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_text("hi").with_tool_request(
                "t1",
                "signal_reply",
                json!({"text": "hi"}),
            ),
            Message::user().with_content(Content::tool_result("t1", "ok", false)),
            // Thinking-only assistant response — no visible content after filtering
            Message::assistant().with_content(Content::Thinking {
                thinking: "internal reasoning".into(),
                signature: None,
            }),
            Message::user().with_text("who are you"),
        ];

        let formatted = AnthropicProvider::format_messages(&messages, false, None);

        // The thinking-only message must be absent
        assert_eq!(formatted.len(), 4);
        for msg in &formatted {
            let content = msg["content"]
                .as_array()
                .expect("content should be an array");
            assert!(!content.is_empty(), "no message should have empty content");
        }
    }

    #[test]
    fn format_messages_keeps_non_empty_assistant_messages() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_text("world"),
        ];

        let formatted = AnthropicProvider::format_messages(&messages, false, None);

        assert_eq!(formatted.len(), 2);
        assert_eq!(formatted[1]["role"], "assistant");
        assert_eq!(formatted[1]["content"][0]["text"], "world");
    }

    #[test]
    fn format_messages_sets_cache_control_on_second_to_last() {
        let messages = vec![
            Message::user().with_text("first"),
            Message::assistant().with_text("second"),
            Message::user().with_text("third"),
        ];

        // cache_at = 1 (second-to-last of 3 messages, index 1)
        let formatted = AnthropicProvider::format_messages(&messages, false, Some(1));

        assert_eq!(formatted.len(), 3);
        // Second message (index 1) should have cache_control on last content block
        assert_eq!(
            formatted[1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // First and third should not
        assert!(formatted[0]["content"][0]["cache_control"].is_null());
        assert!(formatted[2]["content"][0]["cache_control"].is_null());
    }

    #[test]
    fn format_messages_no_cache_on_single_message() {
        let messages = vec![Message::user().with_text("only one")];

        let formatted = AnthropicProvider::format_messages(&messages, false, None);

        assert_eq!(formatted.len(), 1);
        assert!(formatted[0]["content"][0]["cache_control"].is_null());
    }

    #[test]
    fn format_tools_sets_cache_on_last_tool() {
        let tools = vec![
            ToolDef::new("bash", "Run a command", json!({"type": "object"})),
            ToolDef::new("read_file", "Read a file", json!({"type": "object"})),
        ];

        let formatted = AnthropicProvider::format_tools(&tools, false);

        assert_eq!(formatted.len(), 2);
        assert!(formatted[0]["cache_control"].is_null());
        assert_eq!(formatted[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn build_system_blocks_non_oauth_has_cache_control() {
        let blocks = AnthropicProvider::build_system_blocks("You are a test agent.", false);

        let arr = blocks.as_array().expect("should be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(arr[0]["text"], "You are a test agent.");
    }

    #[test]
    fn parse_usage_includes_cache_tokens() {
        let response = AnthropicResponse {
            content: vec![],
            usage: ApiUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: Some(200),
                cache_read_input_tokens: Some(300),
            },
            stop_reason: Some("end_turn".into()),
        };

        let usage = AnthropicProvider::parse_usage(&response);

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_write_tokens, Some(200));
        assert_eq!(usage.cache_read_tokens, Some(300));
    }

    #[test]
    fn parse_usage_handles_missing_cache_fields() {
        let json_str = r#"{
            "content": [],
            "usage": { "input_tokens": 100, "output_tokens": 50 },
            "stop_reason": "end_turn"
        }"#;

        let response: AnthropicResponse = serde_json::from_str(json_str).unwrap();
        let usage = AnthropicProvider::parse_usage(&response);

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_write_tokens, None);
        assert_eq!(usage.cache_read_tokens, None);
    }

    #[test]
    fn format_api_error_parses_rate_limit_json() {
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit."}}"#;
        let result = format_api_error(reqwest::StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(
            result,
            "Rate limited (429 Too Many Requests): Number of request tokens has exceeded your per-minute rate limit."
        );
    }

    #[test]
    fn format_api_error_parses_overloaded_json() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let result = format_api_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, body);
        assert_eq!(
            result,
            "API overloaded (503 Service Unavailable): Overloaded"
        );
    }

    #[test]
    fn format_api_error_parses_auth_error_json() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key."}}"#;
        let result = format_api_error(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(
            result,
            "Authentication failed (401 Unauthorized): Invalid API key."
        );
    }

    #[test]
    fn format_api_error_falls_back_on_non_json() {
        let body = "plain text error";
        let result = format_api_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, body);
        assert_eq!(
            result,
            "Anthropic API error (500 Internal Server Error): plain text error"
        );
    }

    #[test]
    fn format_api_error_falls_back_on_unexpected_json_shape() {
        let body = r#"{"unexpected": "format"}"#;
        let result = format_api_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(
            result,
            r#"Anthropic API error (400 Bad Request): {"unexpected": "format"}"#
        );
    }

    #[test]
    fn format_api_error_handles_unknown_error_type() {
        let body = r#"{"type":"error","error":{"type":"new_error_type","message":"Something new happened."}}"#;
        let result = format_api_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert_eq!(
            result,
            "new_error_type (400 Bad Request): Something new happened."
        );
    }

    #[test]
    fn set_model_updates_model_info() {
        let provider =
            AnthropicProvider::new(vec!["sk-ant-api-test".into()], "claude-sonnet-4-20250514")
                .unwrap();
        assert_eq!(provider.model_info().name, "claude-sonnet-4-20250514");

        provider.set_model("claude-haiku-3-20250514");
        assert_eq!(provider.model_info().name, "claude-haiku-3-20250514");
    }

    #[test]
    fn set_model_strips_anthropic_prefix() {
        let provider =
            AnthropicProvider::new(vec!["sk-ant-api-test".into()], "claude-sonnet-4-20250514")
                .unwrap();

        provider.set_model("anthropic/claude-haiku-3-20250514");
        assert_eq!(provider.model_info().name, "claude-haiku-3-20250514");
    }

    #[test]
    fn set_model_noop_when_same() {
        let provider =
            AnthropicProvider::new(vec!["sk-ant-api-test".into()], "claude-sonnet-4-20250514")
                .unwrap();

        // Same model — should be a no-op (no panic, no change).
        provider.set_model("claude-sonnet-4-20250514");
        assert_eq!(provider.model_info().name, "claude-sonnet-4-20250514");
    }

    #[test]
    fn set_model_affects_build_body() {
        let provider =
            AnthropicProvider::new(vec!["sk-ant-api-test".into()], "claude-sonnet-4-20250514")
                .unwrap();

        let messages = vec![Message::user().with_text("hello")];
        let body = provider.build_body("system", &messages, &[], false, false);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");

        provider.set_model("claude-haiku-3-20250514");
        let body = provider.build_body("system", &messages, &[], false, false);
        assert_eq!(body["model"], "claude-haiku-3-20250514");
    }
}
