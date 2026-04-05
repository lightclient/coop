use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

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
    assert!(
        trace.contains("provider stream item failed") || trace.contains("\"method\":\"stream\"")
    );
}
