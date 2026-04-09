use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};

use coop_agent::{ProviderKind, ProviderSpec, create_provider};
use coop_core::Provider;

use crate::config::Config;
use crate::model_catalog::{
    normalize_model_key, resolve_configured_model, resolve_model_reference,
};
use crate::provider_registry::ProviderRegistry;

pub(crate) fn create_primary_provider(config: &Config) -> Result<Arc<dyn Provider>> {
    create_provider(provider_spec(config, &config.agent.model)?)
}

pub(crate) fn create_provider_for_model(config: &Config, model: &str) -> Result<Arc<dyn Provider>> {
    create_provider(provider_spec(config, model)?)
}

pub(crate) fn build_provider_registry(
    primary: Arc<dyn Provider>,
    config: &Config,
) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new(primary);

    let primary_model = registry.primary().model_info().name;
    let primary_key = normalize_model_key(&primary_model);
    let mut seen = std::collections::HashSet::new();
    for group in &config.groups {
        let requested = resolve_model_reference(config, group.trigger_model_or_default());
        let model = resolve_configured_model(config, &requested.resolved)
            .map(|resolved| resolved.model.id)
            .unwrap_or_else(|| requested.resolved.clone());
        let key = normalize_model_key(&model);

        if key != primary_key && seen.insert(key) {
            match create_provider_for_model(config, &model) {
                Ok(provider) => {
                    info!(
                        provider = provider.name(),
                        model = %model,
                        requested_model = %requested.requested,
                        alias = requested.alias.as_deref().unwrap_or(""),
                        "registered trigger model provider"
                    );
                    registry.register(model, provider);
                }
                Err(error) => {
                    warn!(
                        model = %model,
                        requested_model = %requested.requested,
                        alias = requested.alias.as_deref().unwrap_or(""),
                        error = %error,
                        "failed to create trigger model provider, will use primary"
                    );
                }
            }
        }
    }

    registry
}

pub(crate) fn provider_spec(config: &Config, model: &str) -> Result<ProviderSpec> {
    let requested_model = resolve_model_reference(config, model);
    let resolved_model = resolve_configured_model(config, &requested_model.resolved);
    let default_model = resolve_model_reference(config, &config.agent.model);
    let resolved_default_model = resolve_configured_model(config, &default_model.resolved);
    let provider = if config.providers.is_empty() {
        &config.provider
    } else {
        resolved_model
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "model '{}' is not configured in any provider",
                    requested_model.requested
                )
            })?
            .provider
    };

    let model = resolved_model.as_ref().map_or_else(
        || requested_model.resolved.clone(),
        |resolved| resolved.model.id.clone(),
    );
    let default_model =
        resolved_default_model.map_or(default_model.resolved, |resolved| resolved.model.id);

    let kind = ProviderKind::from_name(&provider.name)?;
    Ok(ProviderSpec {
        kind,
        model,
        default_model: Some(default_model),
        default_model_context_limit: config.agent.context_limit,
        model_context_limits: provider.model_context_limits.clone(),
        api_keys: provider.api_keys.clone(),
        api_key_env: provider.effective_api_key_env(),
        base_url: provider.base_url.clone(),
        extra_headers: provider.extra_headers.clone(),
        refresh_token: provider.refresh_token.clone(),
        reasoning: provider.reasoning.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::Message;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_subscriber::prelude::*;

    fn base_config(provider_name: &str, model: &str) -> Config {
        toml::from_str(&format!(
            "[agent]\nid = \"test\"\nmodel = \"{model}\"\nworkspace = \".\"\n\n[provider]\nname = \"{provider_name}\"\napi_key_env = \"HOME\"\n"
        ))
        .expect("config parses")
    }

    #[test]
    fn create_openai_provider() {
        let config = base_config("openai", "gpt-4o-mini");
        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn create_gemini_provider() {
        let config = base_config("gemini", "gemini-2.5-flash");
        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "gemini");
    }

    #[test]
    fn create_openai_compatible_provider() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"meta-llama/Llama-3.1-8B-Instruct\"\nworkspace = \".\"\n\n[provider]\nname = \"openai-compatible\"\nbase_url = \"http://localhost:8000/v1\"\n",
        )
        .expect("config parses");
        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "openai-compatible");
    }

    #[test]
    fn provider_spec_uses_exact_configured_model_id_for_openai_compatible() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"demo-model\"\nworkspace = \".\"\n\n[provider]\nname = \"openai-compatible\"\nbase_url = \"http://localhost:8000/v1\"\nmodels = [\"openai/demo-model\"]\n",
        )
        .expect("config parses");

        let spec = provider_spec(&config, &config.agent.model).expect("provider spec resolves");
        assert_eq!(spec.model, "openai/demo-model");
        assert_eq!(spec.default_model.as_deref(), Some("openai/demo-model"));
    }

    #[test]
    fn create_ollama_provider() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"llama3.2\"\nworkspace = \".\"\n\n[provider]\nname = \"ollama\"\n",
        )
        .expect("config parses");
        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn create_primary_provider_from_multiple_providers() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"gpt-5-codex\"\nworkspace = \".\"\n\n[[providers]]\nname = \"anthropic\"\nmodels = [\"anthropic/claude-sonnet-4-20250514\"]\napi_key_env = \"HOME\"\n\n[[providers]]\nname = \"openai\"\nmodels = [\"gpt-5-codex\"]\napi_key_env = \"HOME\"\n",
        )
        .expect("config parses");

        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn create_primary_provider_from_alias() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"main\"\nworkspace = \".\"\n\n[models.aliases]\nmain = \"gpt-5-codex\"\n\n[[providers]]\nname = \"anthropic\"\nmodels = [\"anthropic/claude-sonnet-4-20250514\"]\napi_key_env = \"HOME\"\n\n[[providers]]\nname = \"openai\"\nmodels = [\"gpt-5-codex\"]\napi_key_env = \"HOME\"\n",
        )
        .expect("config parses");

        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.model_info().name, "gpt-5-codex");
    }

    #[test]
    fn provider_spec_includes_openai_reasoning_config() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"gpt-5.4\"\nworkspace = \".\"\n\n[provider]\nname = \"openai\"\nmodels = [\"gpt-5.4\"]\n\n[provider.reasoning]\neffort = \"high\"\nsummary = \"concise\"\n",
        )
        .expect("config parses");

        let spec = provider_spec(&config, "gpt-5.4").expect("provider spec");
        let reasoning = spec.reasoning.expect("reasoning config present");
        assert_eq!(
            reasoning.effort,
            Some(coop_agent::OpenAiReasoningEffort::High)
        );
        assert_eq!(
            reasoning.summary,
            Some(coop_agent::OpenAiReasoningSummary::Concise)
        );
    }

    async fn spawn_openai_compatible_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let request = read_http_request(&mut socket).await;
                let is_models =
                    request.starts_with("GET /v1/models ") || request.starts_with("GET /models ");

                let body = if is_models {
                    serde_json::json!({
                        "data": [
                            {"id": "demo-model", "context_length": 128_000}
                        ]
                    })
                } else {
                    serde_json::json!({
                        "id": "chatcmpl_test",
                        "object": "chat.completion",
                        "model": "demo-model",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "TRACE_OK"},
                            "finish_reason": "stop"
                        }],
                        "usage": {
                            "prompt_tokens": 12,
                            "completion_tokens": 4,
                            "total_tokens": 16
                        }
                    })
                }
                .to_string();

                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });

        format!("http://{addr}/v1")
    }

    async fn spawn_gemini_server() -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            let _ = tx.send(request);

            let body = serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [{"text": "TRACE_OK"}]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 12,
                    "candidatesTokenCount": 4,
                    "totalTokenCount": 16
                }
            })
            .to_string();

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        (format!("http://{addr}/v1beta"), rx)
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let mut expected_len = None;
        let mut header_end = None;

        loop {
            let read = socket.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);

            if header_end.is_none()
                && let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            {
                header_end = Some(pos + 4);
                let headers = String::from_utf8_lossy(&buffer[..pos + 4]);
                expected_len = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                });
            }

            if let Some(end) = header_end {
                let content_length = expected_len.unwrap_or(0);
                if buffer.len() >= end + content_length {
                    break;
                }
            }
        }

        String::from_utf8_lossy(&buffer).into_owned()
    }

    #[tokio::test]
    async fn openai_compatible_provider_writes_trace_fields() {
        let base_url = spawn_openai_compatible_server().await;
        let config: Config = toml::from_str(&format!(
            "[agent]\nid = \"test\"\nmodel = \"demo-model\"\nworkspace = \".\"\n\n[provider]\nname = \"openai-compatible\"\nbase_url = \"{base_url}\"\napi_key_env = \"HOME\"\n"
        ))
        .expect("config parses");
        let provider = create_primary_provider(&config).expect("provider creates");

        let trace_path = std::env::var("COOP_TRACE_FILE").map_or_else(
            |_| PathBuf::from("/tmp/coop-provider-trace.jsonl"),
            PathBuf::from,
        );
        let _ = std::fs::remove_file(&trace_path);
        if let Some(parent) = trace_path.parent() {
            std::fs::create_dir_all(parent).expect("create trace dir");
        }

        let trace_parent = trace_path.parent().expect("trace parent");
        let trace_name = trace_path
            .file_name()
            .expect("trace name")
            .to_string_lossy()
            .to_string();
        let file_appender = tracing_appender::rolling::never(trace_parent, trace_name);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));
        let subscriber = tracing_subscriber::Registry::default().with(layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let (response, usage) = provider
            .complete(
                &["Answer tersely".to_owned()],
                &[Message::user().with_text("Reply with TRACE_OK")],
                &[],
            )
            .await
            .expect("provider request succeeds");
        assert_eq!(response.text(), "TRACE_OK");
        assert_eq!(usage.input_tokens, Some(12));

        drop(default_guard);
        drop(guard);

        let trace = std::fs::read_to_string(trace_path).expect("read trace file");
        assert!(trace.contains("provider_request"));
        assert!(trace.contains("openai-compatible"));
        assert!(trace.contains("genai complete response"));
    }

    #[tokio::test]
    async fn gemini_provider_completes_against_native_server() {
        let (base_url, request_rx) = spawn_gemini_server().await;
        let config: Config = toml::from_str(&format!(
            "[agent]\nid = \"test\"\nmodel = \"gemini-2.5-flash\"\ncontext_limit = 1048576\nworkspace = \".\"\n\n[provider]\nname = \"gemini\"\nbase_url = \"{base_url}\"\napi_key_env = \"HOME\"\n"
        ))
        .expect("config parses");
        let provider = create_primary_provider(&config).expect("provider creates");

        let (response, usage) = provider
            .complete(
                &["Answer tersely".to_owned()],
                &[Message::user().with_text("Reply with TRACE_OK")],
                &[],
            )
            .await
            .expect("provider request succeeds");

        assert_eq!(response.text(), "TRACE_OK");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(4));

        let request = request_rx.await.expect("request received");
        assert!(request.contains("POST /v1beta/models/gemini-2.5-flash:generateContent"));
        assert!(request.to_ascii_lowercase().contains("x-goog-api-key:"));
    }

    #[tokio::test]
    async fn openai_compatible_provider_succeeds_without_api_key() {
        let base_url = spawn_openai_compatible_server().await;
        let config: Config = toml::from_str(&format!(
            "[agent]\nid = \"test\"\nmodel = \"demo-model\"\nworkspace = \".\"\n\n[provider]\nname = \"openai-compatible\"\nbase_url = \"{base_url}\"\n",
        ))
        .expect("config parses");
        let provider = create_primary_provider(&config).expect("provider creates");

        let (response, usage) = provider
            .complete(
                &["Answer tersely".to_owned()],
                &[Message::user().with_text("Reply with TRACE_OK")],
                &[],
            )
            .await
            .expect("provider request succeeds");
        assert_eq!(response.text(), "TRACE_OK");
        assert_eq!(usage.input_tokens, Some(12));
    }
}
