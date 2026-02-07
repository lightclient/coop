use std::io::BufRead;

use coop_core::tools::DefaultExecutor;
use coop_core::{ToolContext, ToolExecutor, TrustLevel};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

/// Run tool execution with tracing captured to a temp file,
/// then assert key spans appear in the JSONL output.
#[tokio::test]
async fn tool_execution_produces_expected_spans() {
    let dir = tempfile::tempdir().unwrap();
    let trace_file = dir.path().join("traces.jsonl");

    let file_appender = tracing_appender::rolling::never(dir.path(), "traces.jsonl");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let jsonl_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_span_list(true)
        .with_file(true)
        .with_line_number(true)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_filter(tracing_subscriber::EnvFilter::new("debug"));

    let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let default_guard = tracing::dispatcher::set_default(&dispatch);

    let executor = DefaultExecutor::new();
    let workspace = dir.path().to_path_buf();
    std::fs::write(workspace.join("test.txt"), "hello world").unwrap();

    let ctx = ToolContext {
        session_id: "test-session".to_string(),
        trust: TrustLevel::Full,
        workspace,
    };

    let _result = executor
        .execute("read_file", serde_json::json!({"path": "test.txt"}), &ctx)
        .await
        .unwrap();

    // Drop dispatch and flush
    drop(default_guard);
    drop(guard);

    let file = std::fs::File::open(&trace_file).unwrap();
    let lines: Vec<String> = std::io::BufReader::new(file)
        .lines()
        .map(|l| l.unwrap())
        .filter(|l| !l.is_empty())
        .collect();

    assert!(!lines.is_empty(), "trace file should not be empty");

    // Verify all lines are valid JSON
    for line in &lines {
        let _: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline: {line}"));
    }

    let all_text = lines.join("\n");

    assert!(
        all_text.contains("tool_execute"),
        "missing tool_execute span in traces"
    );
    assert!(
        all_text.contains("read_file"),
        "missing read_file tool name in traces"
    );
}
