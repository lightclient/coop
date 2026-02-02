/// Compile-time smoke test: verify we can reference the Goose types we need
/// for the provider-layer integration (see docs/goose-integration.md).
#[cfg(test)]
mod tests {
    use goose::providers::base::Usage;

    #[test]
    fn goose_provider_types_accessible() {
        let usage = Usage::new(Some(100), Some(50), None);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        // total_tokens is auto-calculated when not provided
        assert_eq!(usage.total_tokens, Some(150));
    }

    #[test]
    fn goose_message_constructible() {
        let msg = goose::conversation::message::Message::user().with_text("hello");
        // If this compiles + runs, we can create Goose messages for the provider.
        assert!(!msg.content.is_empty());
    }

    #[test]
    fn rmcp_tool_constructible() {
        let tool = rmcp::model::Tool::new(
            "test_tool",
            "A test tool",
            serde_json::json!({"type": "object"}).as_object().unwrap().clone(),
        );
        assert_eq!(tool.name.as_ref(), "test_tool");
    }
}
