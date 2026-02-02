/// Compile-time smoke tests: verify we can reference the types we need
/// across both Goose and Coop.
#[cfg(test)]
mod tests {
    use coop_core::{Message, Role, ToolDef};
    use crate::convert;

    #[test]
    fn coop_message_roundtrip_through_goose() {
        let msg = Message::assistant()
            .with_text("Hello!")
            .with_tool_request("c1", "bash", serde_json::json!({"command": "ls"}));

        let goose_msg = convert::to_goose_message(&msg);
        let back = convert::from_goose_message(&goose_msg);

        assert_eq!(back.role, Role::Assistant);
        assert_eq!(back.text(), "Hello!");
        assert_eq!(back.tool_requests().len(), 1);
        assert_eq!(back.tool_requests()[0].name, "bash");
    }

    #[test]
    fn tool_def_converts_to_mcp() {
        let def = ToolDef::new(
            "read",
            "Read a file",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );
        let mcp = convert::to_mcp_tool(&def);
        assert_eq!(mcp.name.as_ref(), "read");
    }

    #[test]
    fn goose_provider_usage_converts() {
        let goose_usage = goose::providers::base::ProviderUsage::new(
            "claude-sonnet-4-20250514".to_string(),
            goose::providers::base::Usage::new(Some(100), Some(50), None),
        );
        let usage = convert::from_goose_usage(&goose_usage);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.total_tokens(), 150);
    }

    #[test]
    fn rmcp_tool_constructible() {
        let tool = rmcp::model::Tool::new(
            "test_tool",
            "A test tool",
            serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(tool.name.as_ref(), "test_tool");
    }
}
