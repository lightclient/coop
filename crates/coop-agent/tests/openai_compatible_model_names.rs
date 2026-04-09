use std::collections::BTreeMap;

use coop_agent::{ProviderKind, ProviderSpec, create_provider};
use coop_core::Message;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn preserves_exact_model_name_for_openai_compatible_requests() {
    let (base_url, request_rx) = spawn_openai_compatible_server().await;
    let provider = create_provider(ProviderSpec {
        kind: ProviderKind::OpenAiCompatible,
        model: "openai/demo-model".to_owned(),
        default_model: Some("openai/demo-model".to_owned()),
        default_model_context_limit: Some(128_000),
        model_context_limits: BTreeMap::new(),
        api_keys: Vec::new(),
        api_key_env: None,
        base_url: Some(base_url),
        extra_headers: BTreeMap::new(),
        refresh_token: None,
        reasoning: None,
    })
    .expect("provider creates");

    assert_eq!(provider.model_info().name, "openai/demo-model");

    let (response, usage) = provider
        .complete(
            &["Answer tersely".to_owned()],
            &[Message::user().with_text("Reply with OK")],
            &[],
        )
        .await
        .expect("provider request succeeds");

    assert_eq!(response.text(), "OK");
    assert_eq!(usage.input_tokens, Some(12));

    let request = request_rx.await.expect("request captured");
    let payload = request_json(&request);
    assert_eq!(payload["model"], "openai/demo-model");
}

async fn spawn_openai_compatible_server() -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut capture = Some(tx);

        loop {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_http_request(&mut socket).await;

            let (body, should_stop) =
                if request.starts_with("GET /v1/models ") || request.starts_with("GET /models ") {
                    (
                        serde_json::json!({
                            "data": [{"id": "openai/demo-model", "context_length": 128000}]
                        })
                        .to_string(),
                        false,
                    )
                } else {
                    if let Some(tx) = capture.take() {
                        let _ = tx.send(request);
                    }
                    (
                        serde_json::json!({
                            "id": "chatcmpl_test",
                            "object": "chat.completion",
                            "model": "openai/demo-model",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "OK"},
                                "finish_reason": "stop"
                            }],
                            "usage": {
                                "prompt_tokens": 12,
                                "completion_tokens": 4,
                                "total_tokens": 16
                            }
                        })
                        .to_string(),
                        true,
                    )
                };

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");

            if should_stop {
                break;
            }
        }
    });

    (format!("http://{addr}/v1"), rx)
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

fn request_json(request: &str) -> Value {
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("request body present");
    serde_json::from_str(body).expect("request body is valid json")
}
