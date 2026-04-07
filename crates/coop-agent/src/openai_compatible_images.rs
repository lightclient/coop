use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};
use tracing::{debug, info, instrument};

use crate::gemini_images::{GeminiImageGenerationResult, GeneratedImage, InputImage};
use crate::{ProviderKind, ProviderSpec};

#[instrument(skip(spec, reference_images), fields(model = %spec.model, reference_image_count = reference_images.len()))]
pub async fn generate_openai_compatible_image(
    spec: &ProviderSpec,
    prompt: &str,
    reference_images: &[InputImage],
) -> Result<GeminiImageGenerationResult> {
    if spec.kind != ProviderKind::OpenAiCompatible {
        bail!("image generation currently supports only openai-compatible providers");
    }

    let api_key = spec
        .resolved_api_keys()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no API key configured for image generation"))?;
    let base_url = spec
        .normalized_base_url()
        .ok_or_else(|| anyhow::anyhow!("image generation requires a configured base_url"))?;
    let endpoint = format!("{base_url}chat/completions");

    let mut content = reference_images
        .iter()
        .map(|image| {
            json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.mime_type, image.data),
                }
            })
        })
        .collect::<Vec<_>>();
    content.push(json!({
        "type": "text",
        "text": prompt.trim(),
    }));

    let body = json!({
        "model": spec.model,
        "messages": [{
            "role": "user",
            "content": content,
        }],
        "modalities": ["text", "image"],
    });

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap_or_else(|_| Client::new());
    let mut request = client
        .post(&endpoint)
        .bearer_auth(api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    for (name, value) in &spec.extra_headers {
        request = request.header(name, value);
    }

    let response = request
        .json(&body)
        .send()
        .await
        .with_context(|| format!("failed to send image request to {endpoint}"))?;
    let status = response.status();
    let response_body: Value = response
        .json()
        .await
        .context("failed to decode openai-compatible image response JSON")?;

    if !status.is_success() {
        let message = response_body
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("openai-compatible image request failed");
        bail!("{message}");
    }

    let result = parse_generation_response(&response_body)?;
    debug!(
        image_count = result.images.len(),
        has_text = result.text.as_ref().is_some_and(|text| !text.is_empty()),
        "parsed openai-compatible image response"
    );
    info!(
        image_count = result.images.len(),
        text_len = result.text.as_ref().map_or(0, String::len),
        "openai-compatible image generation complete"
    );
    Ok(result)
}

fn parse_generation_response(body: &Value) -> Result<GeminiImageGenerationResult> {
    let mut text = String::new();
    let mut images = Vec::new();

    if let Some(choices) = body.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let Some(message) = choice.get("message") else {
                continue;
            };

            if let Some(content) = message.get("content") {
                append_text_and_images_from_content(content, &mut text, &mut images)?;
            }

            if let Some(message_images) = message.get("images").and_then(Value::as_array) {
                for image in message_images {
                    if let Some(parsed) = parse_generated_image(image)? {
                        images.push(parsed);
                    }
                }
            }
        }
    }

    if let Some(data) = body.get("data").and_then(Value::as_array) {
        for item in data {
            if let Some(parsed) = parse_generated_image(item)? {
                images.push(parsed);
            }
            if text.trim().is_empty()
                && let Some(revised_prompt) = item.get("revised_prompt").and_then(Value::as_str)
            {
                text.push_str(revised_prompt);
            }
        }
    }

    if images.is_empty() {
        bail!("openai-compatible image response did not include any generated images");
    }

    Ok(GeminiImageGenerationResult {
        text: (!text.trim().is_empty()).then_some(text),
        images,
    })
}

fn append_text_and_images_from_content(
    content: &Value,
    text: &mut String,
    images: &mut Vec<GeneratedImage>,
) -> Result<()> {
    match content {
        Value::String(value) => text.push_str(value),
        Value::Array(items) => {
            for item in items {
                let Some(item_type) = item.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match item_type {
                    "text" => {
                        if let Some(value) = item.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    "image_url" => {
                        if let Some(parsed) = parse_generated_image(item)? {
                            images.push(parsed);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_generated_image(value: &Value) -> Result<Option<GeneratedImage>> {
    if let Some(data) = value
        .get("b64_json")
        .and_then(Value::as_str)
        .or_else(|| value.get("base64").and_then(Value::as_str))
        .or_else(|| value.get("data").and_then(Value::as_str))
    {
        let mime_type = value
            .get("mime_type")
            .or_else(|| value.get("media_type"))
            .and_then(Value::as_str)
            .filter(|mime| !mime.trim().is_empty())
            .unwrap_or("image/png");

        return Ok(Some(GeneratedImage {
            mime_type: mime_type.to_owned(),
            data: data.to_owned(),
        }));
    }

    let url = value
        .get("image_url")
        .and_then(|image_url| {
            image_url
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| image_url.as_str())
        })
        .or_else(|| value.get("url").and_then(Value::as_str));
    let Some(url) = url else {
        return Ok(None);
    };

    let Some(payload) = url.strip_prefix("data:") else {
        bail!("generated image URL was not a data URL");
    };
    let Some((meta, data)) = payload.split_once(',') else {
        bail!("generated image data URL was malformed");
    };
    if !meta.contains(";base64") {
        bail!("generated image data URL was not base64 encoded");
    }
    let mime_type = meta
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("image/png");

    Ok(Some(GeneratedImage {
        mime_type: mime_type.to_owned(),
        data: data.to_owned(),
    }))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn spec(base_url: String) -> ProviderSpec {
        ProviderSpec {
            kind: ProviderKind::OpenAiCompatible,
            model: "google/gemini-3-pro-image-preview".to_owned(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: std::collections::BTreeMap::default(),
            api_keys: vec!["env:HOME".to_owned()],
            api_key_env: None,
            base_url: Some(base_url),
            extra_headers: std::collections::BTreeMap::default(),
            refresh_token: None,
            reasoning: None,
        }
    }

    #[test]
    fn parses_generated_images_from_images_array() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": "done",
                    "images": [{
                        "type": "image_url",
                        "image_url": {"url": "data:image/png;base64,YWJj"}
                    }]
                }
            }]
        });
        let parsed = parse_generation_response(&body).unwrap();
        assert_eq!(parsed.text.as_deref(), Some("done"));
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].mime_type, "image/png");
    }

    #[test]
    fn parses_generated_images_from_content_array() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": [
                        {"type": "text", "text": "done"},
                        {
                            "type": "image_url",
                            "image_url": {"url": "data:image/png;base64,YWJj"}
                        }
                    ]
                }
            }]
        });
        let parsed = parse_generation_response(&body).unwrap();
        assert_eq!(parsed.text.as_deref(), Some("done"));
        assert_eq!(parsed.images.len(), 1);
    }

    #[test]
    fn parses_generated_images_from_data_array() {
        let body = json!({
            "data": [{
                "b64_json": "YWJj",
                "revised_prompt": "edited",
                "mime_type": "image/png"
            }]
        });

        let parsed = parse_generation_response(&body).unwrap();
        assert_eq!(parsed.text.as_deref(), Some("edited"));
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(parsed.images[0].mime_type, "image/png");
        assert_eq!(parsed.images[0].data, "YWJj");
    }

    #[tokio::test]
    async fn generate_openai_compatible_image_sends_modalities_and_parses_images() {
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

        let result = generate_openai_compatible_image(
            &spec(format!("http://{addr}/v1")),
            "generate from refs",
            &[InputImage {
                mime_type: "image/png".to_owned(),
                data: "YWJj".to_owned(),
            }],
        )
        .await
        .unwrap();

        let request = rx.await.expect("request");
        assert!(request.starts_with("POST /v1/chat/completions "));
        assert!(request.contains("\"modalities\":[\"text\",\"image\"]"));
        assert!(request.contains("data:image/png;base64,YWJj"));
        let image_pos = request.find("\"type\":\"image_url\"").unwrap();
        let text_pos = request.find("\"type\":\"text\"").unwrap();
        assert!(image_pos < text_pos);
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
