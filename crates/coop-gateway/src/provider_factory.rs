use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};

use coop_agent::{ProviderKind, ProviderSpec, create_provider};
use coop_core::Provider;

use crate::config::Config;
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
    let mut seen = std::collections::HashSet::new();
    for group in &config.groups {
        let model = group.trigger_model_or_default();
        if model != primary_model && seen.insert(model.to_owned()) {
            match create_provider_for_model(config, model) {
                Ok(provider) => {
                    info!(
                        provider = provider.name(),
                        model = model,
                        "registered trigger model provider"
                    );
                    registry.register(model.to_owned(), provider);
                }
                Err(error) => {
                    warn!(
                        model = model,
                        error = %error,
                        "failed to create trigger model provider, will use primary"
                    );
                }
            }
        }
    }

    registry
}

fn provider_spec(config: &Config, model: &str) -> Result<ProviderSpec> {
    let kind = ProviderKind::from_name(&config.provider.name)?;
    Ok(ProviderSpec {
        kind,
        model: model.to_owned(),
        api_keys: config.provider.api_keys.clone(),
        api_key_env: config.provider.effective_api_key_env(),
        base_url: config.provider.base_url.clone(),
        extra_headers: config.provider.extra_headers.clone(),
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
    fn create_openai_compatible_provider() {
        let config: Config = toml::from_str(
            "[agent]\nid = \"test\"\nmodel = \"meta-llama/Llama-3.1-8B-Instruct\"\nworkspace = \".\"\n\n[provider]\nname = \"openai-compatible\"\nbase_url = \"http://localhost:8000/v1\"\n",
        )
        .expect("config parses");
        let provider = create_primary_provider(&config).expect("provider creates");
        assert_eq!(provider.name(), "openai-compatible");
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

    async fn spawn_openai_compatible_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _request = read_http_request(&mut socket).await;

            let body = serde_json::json!({
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

        format!("http://{addr}/v1")
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
}
