#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

#[path = "../src/tracing_setup.rs"]
mod tracing_setup;

use coop_core::tools::ReadFileTool;
use coop_core::{SessionKind, Tool, ToolContext, TrustLevel};

#[tokio::test]
async fn scope_resolution_and_denials_are_written_to_jsonl_trace() {
    let trace_dir = tempfile::tempdir().unwrap();
    let trace_path = trace_dir.path().join("traces.jsonl");

    let guard = tracing_setup::init_with_trace_file(false, Some(trace_path.clone()));

    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join("users/alice")).unwrap();
    std::fs::write(workspace.path().join("users/alice/secret.txt"), "secret").unwrap();

    let ctx = ToolContext::new(
        "trace-session",
        SessionKind::Dm("signal:bob-uuid".to_owned()),
        TrustLevel::Inner,
        workspace.path(),
        Some("bob"),
    );

    let tool = ReadFileTool;
    let output = tool
        .execute(serde_json::json!({"path": "../alice/secret.txt"}), &ctx)
        .await
        .unwrap();

    assert!(output.is_error);

    drop(guard);

    let trace = std::fs::read_to_string(&trace_path).unwrap();
    assert!(trace.contains("resolved workspace scope"));
    assert!(trace.contains("workspace scope denied access"));
    assert!(trace.contains("users/bob/"));
    assert!(trace.contains("../alice/secret.txt"));
}
