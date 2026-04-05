use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use tracing::{debug, info, instrument};

use crate::ProviderSpec;
use crate::provider_spec::ProviderKind;

const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputImage {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiImageGenerationResult {
    pub text: Option<String>,
    pub images: Vec<GeneratedImage>,
}

#[instrument(skip(spec, reference_images), fields(model = %spec.model, reference_image_count = reference_images.len()))]
pub async fn generate_gemini_image(
    spec: &ProviderSpec,
    prompt: &str,
    reference_images: &[InputImage],
) -> Result<GeminiImageGenerationResult> {
    if spec.kind != ProviderKind::Gemini {
        bail!("image generation currently supports only gemini providers");
    }

    let api_key = spec
        .resolved_api_keys()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no GEMINI_API_KEY configured for image generation"))?;
    let model = spec.model.strip_prefix("gemini/").unwrap_or(&spec.model);
    let base_url = spec
        .normalized_base_url()
        .unwrap_or_else(|| DEFAULT_GEMINI_BASE_URL.to_owned());
    let endpoint = format!("{base_url}models/{model}:generateContent");

    let mut parts = vec![json!({ "text": prompt.trim() })];
    parts.extend(reference_images.iter().map(|image| {
        json!({
            "inline_data": {
                "mime_type": image.mime_type,
                "data": image.data,
            }
        })
    }));

    let body = json!({
        "contents": [{
            "role": "user",
            "parts": parts,
        }],
        "generationConfig": {
            "responseModalities": ["TEXT", "IMAGE"],
        }
    });

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap_or_else(|_| Client::new());
    let response = client
        .post(&endpoint)
        .query(&[("key", api_key.as_str())])
        .json(&body)
        .send()
        .await
        .with_context(|| format!("failed to send Gemini image request to {endpoint}"))?;
    let status = response.status();
    let response_body: Value = response
        .json()
        .await
        .context("failed to decode Gemini image response JSON")?;

    if !status.is_success() {
        let message = response_body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Gemini image request failed");
        bail!("{message}");
    }

    let result = parse_generation_response(&response_body)?;
    debug!(
        image_count = result.images.len(),
        has_text = result.text.as_ref().is_some_and(|text| !text.is_empty()),
        "parsed Gemini image response"
    );
    info!(
        image_count = result.images.len(),
        text_len = result.text.as_ref().map_or(0, String::len),
        "gemini image generation complete"
    );
    Ok(result)
}

fn parse_generation_response(body: &Value) -> Result<GeminiImageGenerationResult> {
    let candidates = body
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Gemini response missing candidates"))?;

    let mut text = String::new();
    let mut images = Vec::new();

    for candidate in candidates {
        let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        for part in parts {
            if let Some(value) = part.get("text").and_then(Value::as_str) {
                text.push_str(value);
                continue;
            }

            let Some(inline_data) = part.get("inlineData").or_else(|| part.get("inline_data"))
            else {
                continue;
            };
            let Some(mime_type) = inline_data
                .get("mimeType")
                .or_else(|| inline_data.get("mime_type"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(data) = inline_data.get("data").and_then(Value::as_str) else {
                continue;
            };
            images.push(GeneratedImage {
                mime_type: mime_type.to_owned(),
                data: data.to_owned(),
            });
        }
    }

    if images.is_empty() {
        bail!("Gemini response did not include any generated images");
    }

    Ok(GeminiImageGenerationResult {
        text: (!text.trim().is_empty()).then_some(text),
        images,
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_spec::ProviderKind;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn spec(base_url: String) -> ProviderSpec {
        ProviderSpec {
            kind: ProviderKind::Gemini,
            model: "gemini-2.0-flash-preview-image-generation".to_owned(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: std::collections::BTreeMap::default(),
            api_keys: vec!["env:HOME".to_owned()],
            api_key_env: None,
            base_url: Some(base_url),
            extra_headers: std::collections::BTreeMap::default(),
            refresh_token: None,
        }
    }

    #[tokio::test]
    async fn parses_generated_images_from_response() {
        let body = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "done"},
                        {"inlineData": {"mimeType": "image/png", "data": "YWJj"}}
                    ]
                }
            }]
        });
        let parsed = parse_generation_response(&body).unwrap();
        assert_eq!(parsed.text.as_deref(), Some("done"));
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].mime_type, "image/png");
    }

    #[tokio::test]
    async fn generate_gemini_image_sends_response_modalities_and_parses_images() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;
            let _ = tx.send(request);

            let body = json!({
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

        let result = generate_gemini_image(
            &spec(format!("http://{addr}/v1beta")),
            "generate from refs",
            &[InputImage {
                mime_type: "image/png".to_owned(),
                data: "YWJj".to_owned(),
            }],
        )
        .await
        .unwrap();

        let request = rx.await.expect("request");
        assert!(request.starts_with(
            "POST /v1beta/models/gemini-2.0-flash-preview-image-generation:generateContent?key="
        ));
        assert!(request.contains("\"responseModalities\":[\"TEXT\",\"IMAGE\"]"));
        assert!(request.contains("\"inline_data\""));
        assert_eq!(result.text.as_deref(), Some("generated"));
        assert_eq!(result.images.len(), 1);
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
