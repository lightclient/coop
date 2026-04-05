use anyhow::{Context, Result};
use async_trait::async_trait;
use genai::Client;
use genai::chat::ChatOptions;
use std::sync::RwLock;
use std::time::Duration;
use tracing::{Instrument, debug, info, info_span, warn};

use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Message, ModelInfo, ToolDef, Usage};

use crate::key_pool::KeyPool;
use crate::message_mapping::{build_chat_request, map_response_message};
use crate::model_context::{ContextLimitInput, resolve_context_limit};
use crate::model_mapping::ResolvedModel;
use crate::provider_spec::{ProviderKind, ProviderSpec};
use crate::request_trace::{
    ProviderTrace, RequestTrace, summarize_chat_request, summarize_provider_trace,
    summarize_transport_error,
};
use crate::stream_mapping::into_provider_stream;
use crate::transport_probe::{
    build_probe_client, probe_socket_transport_failure, probe_transport_failure,
};
use crate::usage_mapping::usage_from_response;

const MAX_RETRIES: u32 = 3;

pub(crate) struct GenAiProvider {
    client: Client,
    probe_client: reqwest::Client,
    kind: ProviderKind,
    keys: Option<KeyPool>,
    spec: RwLock<ProviderSpec>,
    model: RwLock<ModelInfo>,
}

impl GenAiProvider {
    pub(crate) fn new(spec: ProviderSpec) -> Result<Self> {
        anyhow::ensure!(
            spec.kind != ProviderKind::Anthropic,
            "anthropic should use the compatibility provider"
        );

        let keys = resolve_keys(&spec)?;
        let resolved =
            ResolvedModel::from_spec(&spec, &spec.model, genai::resolver::AuthData::None);
        let context_limit = resolve_context_limit(ContextLimitInput {
            kind: spec.kind,
            model: &spec.model,
            base_url: spec.base_url.as_deref(),
            api_key: first_api_key(keys.as_ref()),
            configured_limit: spec.configured_context_limit(&spec.model),
        });
        let client = Client::builder()
            .with_web_config(genai::WebConfig::default().with_timeout(Duration::from_secs(300)))
            .build();
        let probe_client = build_probe_client()?;

        Ok(Self {
            client,
            probe_client,
            kind: spec.kind,
            keys,
            spec: RwLock::new(spec),
            model: RwLock::new(ModelInfo {
                name: resolved.model_info_name,
                context_limit,
            }),
        })
    }

    fn model_snapshot(&self) -> ModelInfo {
        self.model.read().expect("model lock poisoned").clone()
    }

    fn spec_snapshot(&self) -> ProviderSpec {
        self.spec.read().expect("spec lock poisoned").clone()
    }

    fn chat_options(&self, stream: bool) -> Option<ChatOptions> {
        let spec = self.spec_snapshot();
        let mut options = ChatOptions::default().with_normalize_reasoning_content(true);
        let mut changed = true;

        if stream {
            options = options
                .with_capture_usage(true)
                .with_capture_content(true)
                .with_capture_tool_calls(true)
                .with_capture_reasoning_content(true);
        }

        if !spec.extra_headers.is_empty() {
            let headers = spec
                .extra_headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<Vec<_>>();
            options = options.with_extra_headers(genai::Headers::from(headers));
        }

        if !stream && spec.extra_headers.is_empty() {
            changed = false;
        }

        changed.then_some(options)
    }

    fn request_target(&self) -> (genai::ServiceTarget, Option<usize>, usize, ModelInfo) {
        let spec = self.spec_snapshot();
        let key_count = self.keys.as_ref().map_or(0, KeyPool::len);
        let (auth, key_index) = if let Some(keys) = &self.keys {
            let key_index = keys.best_key();
            let (value, _) = keys.get(key_index);
            (
                genai::resolver::AuthData::from_single(value.to_owned()),
                Some(key_index),
            )
        } else if self.kind == ProviderKind::OpenAiCompatible {
            (genai::resolver::AuthData::from_single(String::new()), None)
        } else {
            (genai::resolver::AuthData::None, None)
        };

        let resolved = ResolvedModel::from_spec(&spec, &spec.model, auth.clone());
        let model_info = self.model_snapshot();

        (
            resolved.to_service_target(auth),
            key_index,
            key_count,
            model_info,
        )
    }

    async fn complete_inner(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let chat_request = build_chat_request(self.kind, system, messages, tools);
        let spec = self.spec_snapshot();
        let provider_trace = summarize_provider_trace(self.kind, &spec, self.keys.is_some());
        let request_trace = summarize_chat_request(&chat_request);
        let options = self.chat_options(false);

        for attempt in 0..=MAX_RETRIES {
            let (target, key_index, key_count, model_info) = self.request_target();
            log_request_start(
                self.name(),
                "complete",
                &model_info.name,
                key_index,
                key_count,
                attempt,
                &provider_trace,
                &request_trace,
            );

            match self
                .client
                .exec_chat(target, chat_request.clone(), options.as_ref())
                .await
            {
                Ok(response) => {
                    let message = map_response_message(
                        &response.content,
                        response.reasoning_content.as_deref(),
                    );
                    let usage = usage_from_response(&response);
                    debug!(
                        provider = self.name(),
                        model = %model_info.name,
                        input_tokens = usage.input_tokens,
                        output_tokens = usage.output_tokens,
                        cache_read_tokens = usage.cache_read_tokens,
                        cache_write_tokens = usage.cache_write_tokens,
                        stop_reason = %usage.stop_reason.as_deref().unwrap_or("unknown"),
                        "genai complete response"
                    );
                    return Ok((message, usage));
                }
                Err(error) => {
                    self.log_request_failure("complete", &model_info.name, attempt, &error)
                        .await;
                    if !self
                        .handle_retryable_error(&error, key_index, key_count, attempt)
                        .await
                    {
                        return Err(anyhow::anyhow!(error.to_string()));
                    }
                }
            }
        }

        anyhow::bail!("provider request failed after {MAX_RETRIES} retries")
    }

    async fn stream_inner(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        let chat_request = build_chat_request(self.kind, system, messages, tools);
        let spec = self.spec_snapshot();
        let provider_trace = summarize_provider_trace(self.kind, &spec, self.keys.is_some());
        let request_trace = summarize_chat_request(&chat_request);
        let options = self.chat_options(true);

        for attempt in 0..=MAX_RETRIES {
            let (target, key_index, key_count, model_info) = self.request_target();
            log_request_start(
                self.name(),
                "stream",
                &model_info.name,
                key_index,
                key_count,
                attempt,
                &provider_trace,
                &request_trace,
            );

            match self
                .client
                .exec_chat_stream(target, chat_request.clone(), options.as_ref())
                .await
            {
                Ok(response) => {
                    return Ok(into_provider_stream(response));
                }
                Err(error) => {
                    self.log_request_failure("stream", &model_info.name, attempt, &error)
                        .await;
                    if !self
                        .handle_retryable_error(&error, key_index, key_count, attempt)
                        .await
                    {
                        return Err(anyhow::anyhow!(error.to_string()));
                    }
                }
            }
        }

        anyhow::bail!("provider stream request failed after {MAX_RETRIES} retries")
    }

    async fn handle_retryable_error(
        &self,
        error: &genai::Error,
        key_index: Option<usize>,
        key_count: usize,
        attempt: u32,
    ) -> bool {
        let Some(key_index) = key_index else {
            return false;
        };

        let Some(keys) = &self.keys else {
            return false;
        };

        let Some(status_data) = status_data(error) else {
            return false;
        };

        if status_data.status == 429 {
            let retry_after = status_data.retry_after.unwrap_or(60);
            keys.mark_rate_limited(key_index, retry_after);
            let next_key = keys.best_key();
            if next_key != key_index && !keys.on_cooldown(next_key) {
                info!(
                    provider = self.name(),
                    old_key = key_index,
                    new_key = next_key,
                    key_count,
                    retry_after,
                    "rate-limited, rotating provider key"
                );
                return true;
            }

            warn!(
                provider = self.name(),
                key_index,
                key_count,
                retry_after,
                "all provider keys are cooling down, waiting before retry"
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
            return attempt < MAX_RETRIES;
        }

        if matches!(status_data.status, 500 | 502 | 503) && attempt < MAX_RETRIES {
            let backoff_ms = (1000u64 * 2u64.pow(attempt)).saturating_add(jitter_ms());
            warn!(
                provider = self.name(),
                key_index,
                key_count,
                attempt = attempt + 1,
                status = status_data.status,
                backoff_ms,
                body = %truncate_body(status_data.body),
                "retryable provider error, backing off"
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            return true;
        }

        false
    }

    fn probe_auth_header(&self, spec: &ProviderSpec) -> Option<String> {
        if spec
            .extra_headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"))
        {
            return None;
        }

        let keys = self.keys.as_ref()?;
        let key_index = keys.best_key();
        let (value, _) = keys.get(key_index);
        Some(format!("Bearer {value}"))
    }

    async fn log_request_failure(
        &self,
        method: &'static str,
        model: &str,
        attempt: u32,
        error: &genai::Error,
    ) {
        let transport = summarize_transport_error(error);
        warn!(
            provider = self.name(),
            method,
            model,
            attempt = attempt + 1,
            transport_error_variant = transport.variant,
            transport_error_kind = transport.kind,
            transport_http_status = transport.http_status,
            transport_reqwest_is_connect = transport.reqwest_is_connect,
            transport_reqwest_is_timeout = transport.reqwest_is_timeout,
            transport_reqwest_is_request = transport.reqwest_is_request,
            transport_reqwest_is_body = transport.reqwest_is_body,
            transport_reqwest_is_decode = transport.reqwest_is_decode,
            transport_url = %transport.url,
            transport_source_chain = %transport.source_chain,
            transport_response_body_excerpt = %transport.body_excerpt,
            error = %error,
            error_debug = ?error,
            "genai request failed"
        );

        let spec = self.spec_snapshot();
        let auth_header = self.probe_auth_header(&spec);
        if let Some(probe) = probe_transport_failure(
            &self.probe_client,
            self.kind,
            &spec,
            &transport,
            auth_header.as_deref(),
        )
        .await
        {
            info!(
                provider = self.name(),
                method,
                model,
                attempt = attempt + 1,
                transport_source_chain = %transport.source_chain,
                transport_probe_target = probe.target,
                transport_probe_kind = probe.kind,
                transport_probe_http_status = probe.http_status,
                transport_probe_reqwest_is_connect = probe.reqwest_is_connect,
                transport_probe_reqwest_is_timeout = probe.reqwest_is_timeout,
                transport_probe_reqwest_is_request = probe.reqwest_is_request,
                transport_probe_reqwest_is_body = probe.reqwest_is_body,
                transport_probe_reqwest_is_decode = probe.reqwest_is_decode,
                transport_probe_url = %probe.url,
                transport_probe_source_chain = %probe.source_chain,
                transport_probe_response_body_excerpt = %probe.body_excerpt,
                "genai transport failure probe complete"
            );
        }

        if let Some(socket_probe) =
            probe_socket_transport_failure(self.kind, &spec, &transport).await
        {
            info!(
                provider = self.name(),
                method,
                model,
                attempt = attempt + 1,
                transport_source_chain = %transport.source_chain,
                transport_socket_probe_target_host = %socket_probe.target_host,
                transport_socket_probe_target_port = socket_probe.target_port,
                transport_socket_probe_resolved_addrs = %socket_probe.resolved_addrs,
                transport_socket_probe_selected_local_addr = %socket_probe.selected_local_addr,
                transport_socket_probe_connect_ok = socket_probe.connect_ok,
                transport_socket_probe_connected_peer_addr = %socket_probe.connected_peer_addr,
                transport_socket_probe_connected_local_addr = %socket_probe.connected_local_addr,
                transport_socket_probe_error_kind = socket_probe.error_kind,
                transport_socket_probe_error = %socket_probe.error,
                "genai socket transport probe complete"
            );
        }
    }
}

impl std::fmt::Debug for GenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model = self.model_snapshot();
        f.debug_struct("GenAiProvider")
            .field("provider", &self.name())
            .field("model", &model.name)
            .field("key_count", &self.keys.as_ref().map_or(0, KeyPool::len))
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Provider for GenAiProvider {
    fn name(&self) -> &str {
        self.kind.as_str()
    }

    fn model_info(&self) -> ModelInfo {
        self.model_snapshot()
    }

    fn set_model(&self, model: &str) {
        let mut spec = self.spec.write().expect("spec lock poisoned");
        if spec.model == model {
            return;
        }
        model.clone_into(&mut spec.model);
        let resolved =
            ResolvedModel::from_spec(&spec, &spec.model, genai::resolver::AuthData::None);
        let context_limit = resolve_context_limit(ContextLimitInput {
            kind: spec.kind,
            model: &spec.model,
            base_url: spec.base_url.as_deref(),
            api_key: first_api_key(self.keys.as_ref()),
            configured_limit: spec.configured_context_limit(&spec.model),
        });
        drop(spec);

        let mut info = self.model.write().expect("model lock poisoned");
        info.name = resolved.model_info_name;
        info.context_limit = context_limit;
        debug!(provider = self.name(), new = %info.name, "provider model updated");
    }

    async fn complete(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let model = self.model_snapshot();
        let key_count = self.keys.as_ref().map_or(0, KeyPool::len);
        let span = info_span!(
            "provider_request",
            provider = self.name(),
            model = %model.name,
            method = "complete",
            message_count = messages.len(),
            tool_count = tools.len(),
            key_count,
        );

        self.complete_inner(system, messages, tools)
            .instrument(span)
            .await
    }

    async fn stream(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        let model = self.model_snapshot();
        let key_count = self.keys.as_ref().map_or(0, KeyPool::len);
        let span = info_span!(
            "provider_request",
            provider = self.name(),
            model = %model.name,
            method = "stream",
            message_count = messages.len(),
            tool_count = tools.len(),
            key_count,
        );

        self.stream_inner(system, messages, tools)
            .instrument(span)
            .await
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn complete_fast(
        &self,
        system: &[String],
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.complete(system, messages, tools).await
    }
}

fn resolve_keys(spec: &ProviderSpec) -> Result<Option<KeyPool>> {
    let keys = spec
        .resolved_api_keys()
        .with_context(|| format!("failed to resolve {} API credentials", spec.name()))?;
    if keys.is_empty() {
        Ok(None)
    } else {
        Ok(Some(KeyPool::new(keys)))
    }
}

fn first_api_key(keys: Option<&KeyPool>) -> Option<&str> {
    let keys = keys?;
    let index = keys.best_key();
    Some(keys.get(index).0)
}

struct StatusData<'a> {
    status: u16,
    body: &'a str,
    retry_after: Option<u64>,
}

fn status_data(error: &genai::Error) -> Option<StatusData<'_>> {
    let genai::Error::WebModelCall { webc_error, .. } = error else {
        return None;
    };
    let genai::webc::Error::ResponseFailedStatus {
        status,
        body,
        headers,
    } = webc_error
    else {
        return None;
    };

    let retry_after = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    Some(StatusData {
        status: status.as_u16(),
        body,
        retry_after,
    })
}

fn jitter_ms() -> u64 {
    u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 500,
    )
}

fn truncate_body(body: &str) -> &str {
    const MAX: usize = 200;
    if body.len() > MAX { &body[..MAX] } else { body }
}

#[allow(clippy::too_many_arguments)]
fn log_request_start(
    provider: &str,
    method: &'static str,
    model: &str,
    key_index: Option<usize>,
    key_count: usize,
    attempt: u32,
    provider_trace: &ProviderTrace,
    request_trace: &RequestTrace,
) {
    debug!(
        provider,
        method,
        model,
        key_index,
        key_count,
        attempt = attempt + 1,
        provider_base_url = %provider_trace.base_url,
        provider_auth_mode = provider_trace.auth_mode,
        provider_extra_header_count = provider_trace.extra_header_count,
        provider_extra_header_names = %provider_trace.extra_header_names,
        mapped_chat_system_count = request_trace.system_count,
        mapped_chat_user_count = request_trace.user_count,
        mapped_chat_assistant_count = request_trace.assistant_count,
        mapped_chat_tool_count = request_trace.tool_count,
        mapped_content_text_count = request_trace.text_part_count,
        mapped_content_binary_count = request_trace.binary_part_count,
        mapped_content_tool_call_count = request_trace.tool_call_part_count,
        mapped_content_tool_response_count = request_trace.tool_response_part_count,
        mapped_content_reasoning_count = request_trace.reasoning_part_count,
        mapped_content_thought_signature_count = request_trace.thought_signature_part_count,
        mapped_assistant_reasoning_message_count = request_trace.assistant_reasoning_message_count,
        mapped_chat_json_bytes = request_trace.json_bytes,
        mapped_chat_json_hash = %request_trace.json_hash,
        mapped_tool_names = %request_trace.tool_names,
        "genai provider request"
    );
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn set_model_updates_model_info() {
        let provider = GenAiProvider::new(ProviderSpec {
            kind: ProviderKind::Ollama,
            model: "llama3.2".into(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: BTreeMap::new(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: None,
            extra_headers: BTreeMap::new(),
            refresh_token: None,
        })
        .unwrap();

        provider.set_model("llama3.3");
        assert_eq!(provider.model_info().name, "llama3.3");
    }

    #[test]
    fn openai_provider_name_matches_kind() {
        let provider = GenAiProvider::new(ProviderSpec {
            kind: ProviderKind::OpenAiCompatible,
            model: "meta-llama/Llama-3.1-8B-Instruct".into(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: BTreeMap::new(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: Some("http://localhost:8000/v1".into()),
            extra_headers: BTreeMap::new(),
            refresh_token: None,
        })
        .unwrap();

        assert_eq!(provider.name(), "openai-compatible");
    }
}
