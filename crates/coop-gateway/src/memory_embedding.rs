use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_memory::EmbeddingProvider;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

use crate::config::MemoryEmbeddingConfig;

#[derive(Debug, Clone, Copy)]
enum ProviderKind {
    OpenAiLike,
    Voyage,
    Cohere,
}

#[derive(Debug, Clone)]
struct ProviderSpec {
    provider_name: String,
    endpoint: String,
    api_key_env: String,
    kind: ProviderKind,
}

impl ProviderSpec {
    fn from_config(config: &MemoryEmbeddingConfig) -> Result<Self> {
        let provider = config.normalized_provider();

        match provider.as_str() {
            "openai" => Ok(Self {
                provider_name: provider,
                endpoint: "https://api.openai.com/v1/embeddings".to_owned(),
                api_key_env: "OPENAI_API_KEY".to_owned(),
                kind: ProviderKind::OpenAiLike,
            }),
            "voyage" => Ok(Self {
                provider_name: provider,
                endpoint: "https://api.voyageai.com/v1/embeddings".to_owned(),
                api_key_env: "VOYAGE_API_KEY".to_owned(),
                kind: ProviderKind::Voyage,
            }),
            "cohere" => Ok(Self {
                provider_name: provider,
                endpoint: "https://api.cohere.com/v2/embed".to_owned(),
                api_key_env: "COHERE_API_KEY".to_owned(),
                kind: ProviderKind::Cohere,
            }),
            "openai-compatible" => {
                let base_url = config
                    .base_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("openai-compatible provider requires memory.embedding.base_url")?;

                anyhow::ensure!(
                    base_url.starts_with("http://") || base_url.starts_with("https://"),
                    "memory.embedding.base_url must start with http:// or https://"
                );

                let api_key_env = config
                    .api_key_env
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .context("openai-compatible provider requires memory.embedding.api_key_env")?;

                Ok(Self {
                    provider_name: provider,
                    endpoint: format!("{}/embeddings", base_url.trim_end_matches('/')),
                    api_key_env: api_key_env.to_owned(),
                    kind: ProviderKind::OpenAiLike,
                })
            }
            _ => anyhow::bail!(
                "unsupported memory embedding provider '{}' (supported: openai, voyage, cohere, openai-compatible)",
                config.provider
            ),
        }
    }
}

#[derive(Debug)]
pub(crate) struct HttpEmbeddingProvider {
    client: Client,
    provider: ProviderSpec,
    model: String,
    dimensions: usize,
    api_key: String,
}

impl HttpEmbeddingProvider {
    pub(crate) fn from_config(config: &MemoryEmbeddingConfig) -> Result<Self> {
        let provider = ProviderSpec::from_config(config)?;
        let api_key = std::env::var(&provider.api_key_env)
            .with_context(|| format!("{} environment variable not set", provider.api_key_env))?;

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to create embedding HTTP client")?;

        Ok(Self {
            client,
            provider,
            model: config.model.clone(),
            dimensions: config.dimensions,
            api_key,
        })
    }

    async fn request_embedding(&self, text: &str) -> Result<Vec<f32>> {
        let body = self.request_body(text);

        debug!(
            provider = %self.provider.provider_name,
            model = %self.model,
            endpoint = %self.provider.endpoint,
            text_len = text.len(),
            "memory embedding request"
        );

        let response = self
            .client
            .post(&self.provider.endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("embedding request failed")?;

        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            warn!(
                provider = %self.provider.provider_name,
                model = %self.model,
                status = %status,
                error_class = "http_status",
                "memory embedding request failed"
            );
            anyhow::bail!("embedding provider error: {status}");
        }

        let embedding = parse_embedding(self.provider.kind, &body_text)?;
        validate_dimensions(self.dimensions, &embedding)?;

        debug!(
            provider = %self.provider.provider_name,
            model = %self.model,
            status = %status,
            dimensions = embedding.len(),
            "memory embedding response"
        );

        Ok(embedding)
    }

    fn request_body(&self, text: &str) -> serde_json::Value {
        match self.provider.kind {
            ProviderKind::OpenAiLike => json!({
                "model": self.model,
                "input": text,
                "dimensions": self.dimensions,
                "encoding_format": "float"
            }),
            ProviderKind::Voyage => json!({
                "model": self.model,
                "input": [text]
            }),
            ProviderKind::Cohere => json!({
                "model": self.model,
                "texts": [text],
                "input_type": "search_document",
                "embedding_types": ["float"]
            }),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbeddingProvider {
    #[instrument(
        skip(self, text),
        fields(provider = %self.provider.provider_name, model = %self.model, text_len = text.len())
    )]
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.request_embedding(text).await
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

pub(crate) fn build_embedder(
    config: Option<&MemoryEmbeddingConfig>,
) -> Result<Option<Arc<dyn EmbeddingProvider>>> {
    let Some(config) = config else {
        return Ok(None);
    };

    let provider = HttpEmbeddingProvider::from_config(config)?;
    Ok(Some(Arc::new(provider)))
}

fn parse_embedding(kind: ProviderKind, body: &str) -> Result<Vec<f32>> {
    match kind {
        ProviderKind::OpenAiLike | ProviderKind::Voyage => {
            let parsed: OpenAiLikeResponse = serde_json::from_str(body)
                .context("failed to parse openai-like embedding response")?;
            parsed
                .data
                .into_iter()
                .next()
                .map(|entry| entry.embedding)
                .context("embedding response missing data")
        }
        ProviderKind::Cohere => {
            let parsed: CohereResponse =
                serde_json::from_str(body).context("failed to parse cohere embedding response")?;
            parsed.embeddings.first_embedding()
        }
    }
}

fn validate_dimensions(expected: usize, embedding: &[f32]) -> Result<()> {
    if embedding.len() != expected {
        anyhow::bail!(
            "embedding dimensions mismatch: expected {}, got {}",
            expected,
            embedding.len()
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct OpenAiLikeResponse {
    data: Vec<OpenAiLikeData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiLikeData {
    embedding: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct CohereResponse {
    embeddings: CohereEmbeddings,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CohereEmbeddings {
    Typed { float: Vec<Vec<f32>> },
    Flat(Vec<Vec<f32>>),
}

impl CohereEmbeddings {
    fn first_embedding(self) -> Result<Vec<f32>> {
        match self {
            Self::Typed { float } => float
                .into_iter()
                .next()
                .context("cohere embedding response missing float values"),
            Self::Flat(values) => values
                .into_iter()
                .next()
                .context("cohere embedding response missing values"),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    const TRACE_FILE_PATH: &str = "/tmp/coop-memory-embedding-trace.jsonl";

    fn config(provider: &str) -> MemoryEmbeddingConfig {
        MemoryEmbeddingConfig {
            provider: provider.to_owned(),
            model: "test-model".to_owned(),
            dimensions: 4,
            base_url: None,
            api_key_env: None,
        }
    }

    #[test]
    fn unsupported_provider_rejected() {
        let err = ProviderSpec::from_config(&config("unknown")).unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported memory embedding provider")
        );
    }

    #[test]
    fn provider_spec_supports_cohere() {
        let spec = ProviderSpec::from_config(&config("cohere")).unwrap();
        assert_eq!(spec.provider_name, "cohere");
        assert_eq!(spec.api_key_env, "COHERE_API_KEY");
    }

    #[test]
    fn openai_compatible_requires_base_url_and_env() {
        let cfg = config("openai-compatible");
        let err = ProviderSpec::from_config(&cfg).unwrap_err();
        assert!(err.to_string().contains("base_url"));

        let mut cfg = config("openai-compatible");
        cfg.base_url = Some("https://example.com/v1".to_owned());
        cfg.api_key_env = Some("OPENAI_COMPAT_KEY".to_owned());

        let spec = ProviderSpec::from_config(&cfg).unwrap();
        assert_eq!(spec.endpoint, "https://example.com/v1/embeddings");
        assert_eq!(spec.api_key_env, "OPENAI_COMPAT_KEY");
    }

    #[test]
    fn parses_openai_like_embedding() {
        let body = r#"{"data":[{"embedding":[0.1,0.2,0.3,0.4]}]}"#;
        let embedding = parse_embedding(ProviderKind::OpenAiLike, body).unwrap();
        assert_eq!(embedding.len(), 4);
    }

    #[test]
    fn parses_cohere_embedding() {
        let body = r#"{"embeddings":{"float":[[0.1,0.2,0.3,0.4]]}}"#;
        let embedding = parse_embedding(ProviderKind::Cohere, body).unwrap();
        assert_eq!(embedding.len(), 4);
    }

    #[test]
    fn dimension_mismatch_is_rejected() {
        let err = validate_dimensions(3, &[0.1, 0.2]).unwrap_err();
        assert!(err.to_string().contains("dimensions mismatch"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn embedding_trace_includes_metadata_and_excludes_api_key() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tracing_subscriber::fmt::format::FmtSpan;
        use tracing_subscriber::prelude::*;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();

            let body = r#"{"data":[{"embedding":[0.1,0.2]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = HttpEmbeddingProvider {
            client: Client::builder().build().unwrap(),
            provider: ProviderSpec {
                provider_name: "openai-compatible".to_owned(),
                endpoint: format!("http://{addr}/embeddings"),
                api_key_env: "IGNORED".to_owned(),
                kind: ProviderKind::OpenAiLike,
            },
            model: "test-model".to_owned(),
            dimensions: 2,
            api_key: "test-token".to_owned(),
        };

        let trace_file = std::env::var("COOP_TRACE_FILE").map_or_else(
            |_| std::path::PathBuf::from(TRACE_FILE_PATH),
            std::path::PathBuf::from,
        );
        let _ = std::fs::remove_file(&trace_file);

        let trace_parent = trace_file.parent().unwrap();
        let trace_name = trace_file
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(trace_parent).unwrap();

        let file_appender = tracing_appender::rolling::never(trace_parent, trace_name);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_span_list(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(layer);
        let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
        let default_guard = tracing::dispatcher::set_default(&dispatch);

        let embedding = provider.embed("hello").await.unwrap();
        assert_eq!(embedding.len(), 2);

        drop(default_guard);
        drop(guard);

        let trace = std::fs::read_to_string(trace_file).unwrap();
        assert!(trace.contains("memory embedding request"));
        assert!(trace.contains("memory embedding response"));
        assert!(!trace.contains("test-token"));
    }
}
