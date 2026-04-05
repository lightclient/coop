#![allow(
    clippy::unwrap_used,
    clippy::print_stderr,
    unsafe_code,
    clippy::default_trait_access
)]
//! Live OpenAI auth-token integration test.
//!
//! Runs only when OPENAI_AUTH_TOKEN is set. This exercises the ChatGPT/Codex
//! OAuth access-token path against the live backend.

use std::sync::Arc;

#[tokio::test]
async fn live_openai_auth_roundtrip() {
    let token = match std::env::var("OPENAI_AUTH_TOKEN") {
        Ok(t) if t.split('.').count() == 3 => t,
        _ => {
            eprintln!("skipping: no OpenAI auth token in OPENAI_AUTH_TOKEN");
            return;
        }
    };

    let spec = coop_agent::ProviderSpec {
        kind: coop_agent::ProviderKind::OpenAi,
        model: "openai/gpt-5.4".to_owned(),
        default_model: None,
        default_model_context_limit: None,
        model_context_limits: Default::default(),
        api_keys: vec![format!("env:OPENAI_AUTH_TOKEN")],
        api_key_env: None,
        base_url: None,
        extra_headers: Default::default(),
        refresh_token: None,
    };

    // Ensure env var is set for resolve_key_refs
    // SAFETY: test runs single-threaded
    unsafe { std::env::set_var("OPENAI_AUTH_TOKEN", &token) };

    let provider: Arc<dyn coop_core::Provider> =
        coop_agent::create_provider(spec).expect("provider creation");

    let system = vec!["You are a precise assistant.".to_owned()];
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
