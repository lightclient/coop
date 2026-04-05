use anyhow::{Result, bail};
use async_trait::async_trait;
use coop_agent::{
    InputImage, ProviderKind, generate_gemini_image, generate_openai_compatible_image,
};
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput};
use coop_core::{SessionKind, TrustLevel, save_base64_image};
use tracing::{debug, info, instrument};

use crate::config::{ModelModality, SharedConfig};
use crate::model_capabilities::model_capabilities;
use crate::model_catalog::{provider_model_candidates, resolve_configured_model};
use crate::provider_factory;

#[allow(missing_debug_implementations)]
pub(crate) struct ImageToolExecutor {
    config: SharedConfig,
}

impl ImageToolExecutor {
    pub(crate) fn new(config: SharedConfig) -> Self {
        Self { config }
    }

    fn image_generate_def() -> ToolDef {
        ToolDef::new(
            "image_generate",
            "Generate or edit images with a configured image-capable model. Supports direct Gemini providers and OpenAI-compatible image models such as OpenRouter-hosted Gemini. Saves generated files in the workspace and returns output paths.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Prompt describing the image to generate or the edit to perform."
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional configured model id. Defaults to the current session model when it supports image output."
                    },
                    "reference_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional workspace file paths for reference images or image-to-image editing."
                    },
                    "output_dir": {
                        "type": "string",
                        "description": "Optional output directory inside the workspace. Defaults to generated/images or generated/subagents/<run_id>."
                    },
                    "file_stem": {
                        "type": "string",
                        "description": "Optional output filename stem. Defaults to image."
                    }
                },
                "required": ["prompt"]
            }),
        )
    }

    fn has_image_models(&self) -> bool {
        let config = self.config.load();
        configured_image_models(&config).next().is_some()
    }

    #[allow(clippy::too_many_lines)]
    #[instrument(skip(self, ctx, arguments), fields(session = %ctx.session_id, session_kind = ?ctx.session_kind))]
    async fn handle_image_generate(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "image_generate tool requires Full or Inner trust level",
            ));
        }

        let prompt = arguments
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing 'prompt' parameter"))?;
        let requested_model = arguments
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let output_dir = arguments
            .get("output_dir")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map_or_else(|| default_output_dir(&ctx.session_kind), str::to_owned);
        let file_stem = arguments
            .get("file_stem")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map_or_else(|| "image".to_owned(), sanitize_file_stem);
        let reference_paths = arguments
            .get("reference_paths")
            .and_then(serde_json::Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let config = self.config.load();
        let target_model = resolve_target_model(&config, ctx, requested_model.as_deref())?;
        let capabilities = model_capabilities(&config, &target_model)
            .ok_or_else(|| anyhow::anyhow!("unknown image model: {target_model}"))?;
        if capabilities.subagent_only && !matches!(ctx.session_kind, SessionKind::Subagent(_)) {
            bail!("model '{target_model}' is subagent-only; use it from a subagent profile");
        }
        if !capabilities.supports_output(ModelModality::Image) {
            bail!("model '{target_model}' does not support image output");
        }

        let resolved = resolve_configured_model(&config, &target_model)
            .ok_or_else(|| anyhow::anyhow!("model '{target_model}' is not configured"))?;
        let provider_kind = ProviderKind::from_name(&resolved.provider.name)?;

        let mut input_images = Vec::new();
        for path in &reference_paths {
            let (data, mime_type) = coop_core::images::load_image(path, &ctx.workspace_scope)?;
            input_images.push(InputImage { mime_type, data });
        }

        let spec = provider_factory::provider_spec(&config, &target_model)?;
        let result = match provider_kind {
            ProviderKind::Gemini => generate_gemini_image(&spec, prompt, &input_images).await?,
            ProviderKind::OpenAiCompatible => {
                generate_openai_compatible_image(&spec, prompt, &input_images).await?
            }
            _ => {
                bail!(
                    "image_generate currently supports only gemini and openai-compatible providers; '{}' is backed by {}",
                    target_model,
                    resolved.provider.name
                )
            }
        };

        let output_paths = result
            .images
            .iter()
            .enumerate()
            .map(|(index, image)| {
                save_base64_image(
                    &ctx.workspace_scope,
                    &output_dir,
                    &file_stem,
                    index,
                    &image.data,
                    &image.mime_type,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        info!(
            model = %target_model,
            output_count = output_paths.len(),
            reference_count = reference_paths.len(),
            output_dir = %output_dir,
            "image generation complete"
        );
        debug!(model = %target_model, output_paths = ?output_paths, "saved generated images");

        Ok(ToolOutput::success(
            serde_json::json!({
                "provider": resolved.provider.name,
                "model": target_model,
                "output_paths": output_paths,
                "text": result.text,
                "reference_paths": reference_paths,
            })
            .to_string(),
        ))
    }
}

#[async_trait]
impl ToolExecutor for ImageToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        match name {
            "image_generate" => self.handle_image_generate(arguments, ctx).await,
            _ => Ok(ToolOutput::error(format!("unknown tool: {name}"))),
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        if self.has_image_models() {
            vec![Self::image_generate_def()]
        } else {
            Vec::new()
        }
    }
}

fn configured_image_models(config: &crate::config::Config) -> impl Iterator<Item = String> + '_ {
    config
        .main_provider_configs()
        .into_iter()
        .flat_map(|provider| {
            provider_model_candidates(provider)
                .into_iter()
                .filter(|model| {
                    crate::model_capabilities::provider_model_capabilities(provider, &model.id)
                        .supports_output(ModelModality::Image)
                })
                .map(|model| model.id)
        })
}

fn resolve_target_model(
    config: &crate::config::Config,
    ctx: &ToolContext,
    requested_model: Option<&str>,
) -> Result<String> {
    if let Some(model) = requested_model {
        return Ok(model.to_owned());
    }

    if let Some(model) = ctx.model.as_deref()
        && model_capabilities(config, model)
            .is_some_and(|caps| caps.supports_output(ModelModality::Image))
    {
        return Ok(model.to_owned());
    }

    let mut configured = configured_image_models(config);
    let Some(first) = configured.next() else {
        bail!("no configured models support image output");
    };
    if configured.next().is_none() {
        return Ok(first);
    }

    bail!("multiple image-capable models are configured; pass the 'model' parameter explicitly")
}

fn default_output_dir(session_kind: &SessionKind) -> String {
    match session_kind {
        SessionKind::Subagent(run_id) => format!("generated/subagents/{run_id}"),
        _ => "generated/images".to_owned(),
    }
}

fn sanitize_file_stem(input: &str) -> String {
    let stem: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "image".to_owned()
    } else {
        stem.chars().take(48).collect()
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::shared_config;
    use coop_core::traits::ToolContext;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn default_output_dir_uses_run_id_for_subagents() {
        let run_id = uuid::Uuid::nil();
        assert_eq!(
            default_output_dir(&SessionKind::Subagent(run_id)),
            format!("generated/subagents/{run_id}")
        );
    }

    #[test]
    fn sanitize_file_stem_replaces_unsafe_chars() {
        assert_eq!(sanitize_file_stem("hello world!.png"), "hello-world--png");
    }

    #[test]
    fn tool_is_hidden_without_image_models() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "gpt-5.4"
workspace = "."

[provider]
name = "openai"
models = ["gpt-5.4"]
"#,
        )
        .unwrap();
        let executor = ImageToolExecutor::new(shared_config(cfg));
        assert!(executor.tools().is_empty());
    }

    #[test]
    fn current_model_is_used_when_it_supports_image_output() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
[agent]
id = "test"
model = "gemini-2.0-flash-preview-image-generation"
workspace = "."

[provider]
name = "gemini"
models = ["gemini-2.0-flash-preview-image-generation"]

[provider.model_capabilities."gemini-2.0-flash-preview-image-generation"]
output_modalities = ["text", "image"]
"#,
        )
        .unwrap();
        let ctx = ToolContext::new("session", SessionKind::Main, TrustLevel::Full, ".", None)
            .with_model("gemini-2.0-flash-preview-image-generation");
        let model = resolve_target_model(&cfg, &ctx, None).unwrap();
        assert_eq!(model, "gemini-2.0-flash-preview-image-generation");
    }

    #[tokio::test]
    async fn image_generate_saves_generated_artifacts() {
        let workspace = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _request = read_http_request(&mut socket).await;
            let body = serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            {"text": "generated"},
                            {"inlineData": {"mimeType": "image/png", "data": "YWJj"}}
                        ]
                    }
                }]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });

        let config: crate::config::Config = toml::from_str(&format!(
            r#"
[agent]
id = "test"
model = "gpt-5.4"
workspace = "."

[provider]
name = "gemini"
models = ["gemini-2.0-flash-preview-image-generation"]
base_url = "http://{addr}/v1beta"
api_keys = ["env:HOME"]

[provider.model_capabilities."gemini-2.0-flash-preview-image-generation"]
output_modalities = ["text", "image"]
"#,
        ))
        .unwrap();

        let executor = ImageToolExecutor::new(shared_config(config));
        let ctx = ToolContext::new(
            "session",
            SessionKind::Main,
            TrustLevel::Full,
            workspace.path(),
            Some("alice"),
        )
        .with_model("gemini-2.0-flash-preview-image-generation");

        let output = executor
            .execute(
                "image_generate",
                serde_json::json!({"prompt": "draw a test image"}),
                &ctx,
            )
            .await
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let path = value["output_paths"][0].as_str().unwrap();
        assert!(path.starts_with("./generated/images/image"));
        assert!(workspace.path().join("generated/images/image.png").exists());
        assert_eq!(value["text"], "generated");
    }

    #[tokio::test]
    async fn image_generate_supports_openai_compatible_models() {
        let workspace = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let _request = read_http_request(&mut socket).await;
            let body = serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "generated",
                        "images": [{
                            "type": "image_url",
                            "image_url": {"url": "data:image/png;base64,YWJj"}
                        }]
                    }
                }]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.expect("write");
        });

        let config: crate::config::Config = toml::from_str(&format!(
            r#"
[agent]
id = "test"
model = "gpt-5.4"
workspace = "."

[provider]
name = "openai-compatible"
models = ["google/gemini-3-pro-image-preview"]
base_url = "http://{addr}/v1"
api_keys = ["env:HOME"]

[provider.model_capabilities."google/gemini-3-pro-image-preview"]
output_modalities = ["text", "image"]
"#,
        ))
        .unwrap();

        let executor = ImageToolExecutor::new(shared_config(config));
        let ctx = ToolContext::new(
            "session",
            SessionKind::Main,
            TrustLevel::Full,
            workspace.path(),
            Some("alice"),
        )
        .with_model("google/gemini-3-pro-image-preview");

        let output = executor
            .execute(
                "image_generate",
                serde_json::json!({"prompt": "draw a test image"}),
                &ctx,
            )
            .await
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        let path = value["output_paths"][0].as_str().unwrap();
        assert!(path.starts_with("./generated/images/image"));
        assert!(workspace.path().join("generated/images/image.png").exists());
        assert_eq!(value["text"], "generated");
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
}
