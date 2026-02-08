#![allow(clippy::unwrap_used)]
use std::io::BufRead;

/// Test that the JSONL tracing layer produces valid JSON with expected fields.
#[test]
fn jsonl_layer_produces_valid_json() {
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::prelude::*;

    let dir = tempfile::tempdir().unwrap();
    let trace_file = dir.path().join("test-traces.jsonl");

    let file_appender = tracing_appender::rolling::never(dir.path(), "test-traces.jsonl");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let jsonl_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_span_list(true)
        .with_file(true)
        .with_line_number(true)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE);

    let subscriber = tracing_subscriber::Registry::default().with(jsonl_layer);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("test_span", key = "value");
        let _enter = span.enter();
        tracing::info!(count = 42, "test event");
    });

    // Flush the non-blocking writer
    drop(guard);

    let file = std::fs::File::open(&trace_file).unwrap();
    let lines: Vec<String> = std::io::BufReader::new(file)
        .lines()
        .map(|l| l.unwrap())
        .filter(|l| !l.is_empty())
        .collect();

    // Should have 3 lines: span new, event, span close
    assert!(
        lines.len() >= 2,
        "expected at least 2 JSONL lines, got {}",
        lines.len()
    );

    for line in &lines {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline: {line}"));

        assert!(parsed.get("timestamp").is_some(), "missing timestamp");
        assert!(parsed.get("level").is_some(), "missing level");
    }

    // Find the event line (has "test event" message)
    let event_line = lines
        .iter()
        .find(|l| l.contains("test event"))
        .expect("missing 'test event' line");
    let event: serde_json::Value = serde_json::from_str(event_line).unwrap();

    assert_eq!(
        event["fields"]["count"], 42,
        "expected count=42 in event fields"
    );

    let spans = event["spans"].as_array().expect("missing spans array");
    assert!(!spans.is_empty(), "spans should not be empty");
    assert_eq!(spans[0]["name"], "test_span");
    assert_eq!(spans[0]["key"], "value");
}
