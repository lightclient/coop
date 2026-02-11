#![allow(clippy::unwrap_used, clippy::print_stderr)]
//! Live API integration test — only runs when ANTHROPIC_API_KEY is set.

use std::sync::Arc;

#[tokio::test]
async fn live_oauth_roundtrip() {
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if k.contains("sk-ant-oat") => k,
        _ => {
            eprintln!("skipping: no OAuth token in ANTHROPIC_API_KEY");
            return;
        }
    };

    let provider =
        coop_agent::AnthropicProvider::new(vec![api_key], "anthropic/claude-sonnet-4-20250514")
            .expect("provider creation");
    let provider: Arc<dyn coop_core::Provider> = Arc::new(provider);

    let system = vec!["You are Claude Code, Anthropic's official CLI for Claude.".to_owned()];
    let msg = coop_core::Message::user().with_text("Respond with exactly: COOP_OK");

    let (response, usage) = provider
        .complete(&system, &[msg], &[])
        .await
        .expect("API call");
    let text = response.text();

    eprintln!("Response: {text}");
    eprintln!("Usage: {usage:?}");

    assert!(
        text.contains("COOP_OK"),
        "expected response to contain COOP_OK, got: {text}"
    );
}
