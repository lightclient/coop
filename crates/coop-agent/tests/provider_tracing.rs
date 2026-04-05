use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tracing_subscriber::prelude::*;

use coop_agent::{ProviderKind, ProviderSpec, create_provider};
use coop_core::types::Message;

fn unused_openai_base_url() -> String {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    format!("http://{addr}/v1")
}

fn trace_path(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    std::env::temp_dir().join(format!("coop-agent-{name}-{nanos}.jsonl"))
}

fn start_models_probe_server() -> (String, thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    listener
        .set_nonblocking(true)
        .expect("set test listener nonblocking");
    let handle = thread::spawn(move || {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];

            loop {
                let read = stream.read(&mut buffer).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&request);
            if request.is_empty() {
                continue;
            }

            if request.starts_with("POST /v1/chat/completions ") {
                let partial_response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 64\r\nconnection: close\r\n\r\n{\"id\":\"broken\"";
                stream
                    .write_all(partial_response.as_bytes())
                    .expect("write partial chat response");
            } else if request.starts_with("GET /v1/models ") {
                let body = r#"{"object":"list","data":[{"id":"demo-model"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write models response");
            } else {
                let body = "not found";
                let response = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write not found response");
            }
        }
    });

    (format!("http://{addr}/v1"), handle)
}

#[tokio::test]
async fn provider_trace_logs_request_shape_and_transport_details() {
    let provider = create_provider(ProviderSpec {
        kind: ProviderKind::OpenAiCompatible,
        model: "demo-model".into(),
        default_model: None,
        default_model_context_limit: None,
        model_context_limits: BTreeMap::new(),
        api_keys: Vec::new(),
        api_key_env: None,
        base_url: Some(unused_openai_base_url()),
        extra_headers: BTreeMap::new(),
        refresh_token: None,
    })
    .expect("provider creates");

    let trace_path = trace_path("provider-trace");
    let _ = std::fs::remove_file(&trace_path);
    let trace_file = std::fs::File::create(&trace_path).expect("create trace file");
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(move || trace_file.try_clone().expect("clone trace file"))
        .with_span_list(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
        .with_filter(tracing_subscriber::EnvFilter::new("debug"));
    let subscriber = tracing_subscriber::Registry::default().with(layer);
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let default_guard = tracing::dispatcher::set_default(&dispatch);

    provider
        .complete(
            &["Answer tersely".to_owned()],
            &[Message::user().with_text("Reply with TRACE_OK")],
            &[],
        )
        .await
        .expect_err("request should fail against closed port");

    match provider
        .stream(
            &["Answer tersely".to_owned()],
            &[Message::user().with_text("Reply with TRACE_OK")],
            &[],
        )
        .await
    {
        Ok(mut stream) => {
            stream
                .next()
                .await
                .expect("stream should yield failure")
                .expect_err("stream should fail against closed port");
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("error sending request") || message.contains("Connection refused"),
                "unexpected stream setup error: {message}"
            );
        }
    }

    drop(default_guard);

    let trace = std::fs::read_to_string(&trace_path).expect("read trace file");
    let _ = std::fs::remove_file(&trace_path);

    assert!(trace.contains("genai provider request"));
    assert!(trace.contains("\"provider_base_url\":\"http://127.0.0.1:"));
    assert!(trace.contains("\"provider_auth_mode\":\"empty-bearer\""));
    assert!(trace.contains("\"mapped_chat_json_hash\":"));
    assert!(trace.contains("\"mapped_chat_json_bytes\":"));
    assert!(trace.contains("\"transport_error_kind\":\"reqwest\""));
    assert!(trace.contains("\"transport_reqwest_is_connect\":true"));
    assert!(trace.contains("genai transport failure probe complete"));
    assert!(trace.contains("\"transport_probe_target\":\"models\""));
    assert!(trace.contains("genai socket transport probe complete"));
    assert!(trace.contains("\"transport_socket_probe_connect_ok\":false"));
    assert!(trace.contains("genai command transport probe complete"));
    assert!(trace.contains("\"transport_command_probe_target\":\"curl_models\""));
    assert!(
        trace.contains("provider stream item failed") || trace.contains("\"method\":\"stream\"")
    );
}

#[tokio::test]
async fn provider_failure_probe_can_show_models_endpoint_is_up() {
    let (base_url, server) = start_models_probe_server();
    let provider = create_provider(ProviderSpec {
        kind: ProviderKind::OpenAiCompatible,
        model: "demo-model".into(),
        default_model: None,
        default_model_context_limit: None,
        model_context_limits: BTreeMap::new(),
        api_keys: Vec::new(),
        api_key_env: None,
        base_url: Some(base_url),
        extra_headers: BTreeMap::new(),
        refresh_token: None,
    })
    .expect("provider creates");

    let trace_path = trace_path("provider-probe-success");
    let _ = std::fs::remove_file(&trace_path);
    let trace_file = std::fs::File::create(&trace_path).expect("create trace file");
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(move || trace_file.try_clone().expect("clone trace file"))
        .with_span_list(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
        .with_filter(tracing_subscriber::EnvFilter::new("debug"));
    let subscriber = tracing_subscriber::Registry::default().with(layer);
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let default_guard = tracing::dispatcher::set_default(&dispatch);

    provider
        .complete(
            &["Answer tersely".to_owned()],
            &[Message::user().with_text("Reply with TRACE_OK")],
            &[],
        )
        .await
        .expect_err("chat request should fail when server drops the connection");

    drop(default_guard);
    server.join().expect("server thread joins");

    let trace = std::fs::read_to_string(&trace_path).expect("read trace file");
    let _ = std::fs::remove_file(&trace_path);

    assert!(trace.contains("genai request failed"));
    assert!(trace.contains("genai transport failure probe complete"));
    assert!(trace.contains("\"transport_probe_http_status\":200"));
    assert!(trace.contains("genai socket transport probe complete"));
    assert!(trace.contains("\"transport_socket_probe_connect_ok\":true"));
    assert!(trace.contains("genai command transport probe complete"));
    assert!(trace.contains("\"transport_command_probe_target\":\"route_get\""));
    assert!(trace.contains("/v1/models"));
}
