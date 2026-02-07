pub mod bash;
pub mod list_directory;
pub mod read_file;
pub mod write_file;

use crate::traits::{Tool, ToolContext, ToolExecutor};
use crate::types::{ToolDef, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use tracing::{Instrument, debug, info_span};

pub use bash::BashTool;
pub use list_directory::ListDirectoryTool;
pub use read_file::ReadFileTool;
pub use write_file::WriteFileTool;

/// Production tool executor with all native tools.
#[allow(missing_debug_implementations)]
pub struct DefaultExecutor {
    tools: Vec<Box<dyn Tool>>,
}

impl DefaultExecutor {
    pub fn new() -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(BashTool),
            Box::new(ReadFileTool),
            Box::new(WriteFileTool),
            Box::new(ListDirectoryTool),
        ];
        Self { tools }
    }
}

impl Default for DefaultExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(missing_debug_implementations)]
pub struct CompositeExecutor {
    executors: Vec<Box<dyn ToolExecutor>>,
}

impl CompositeExecutor {
    pub fn new(executors: Vec<Box<dyn ToolExecutor>>) -> Self {
        Self { executors }
    }
}

#[async_trait]
impl ToolExecutor for DefaultExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let span = info_span!("tool_execute", tool = %name);
        async {
            debug!(arguments = %arguments, "tool arguments");
            for tool in &self.tools {
                if tool.definition().name == name {
                    return tool.execute(arguments, ctx).await;
                }
            }
            Ok(ToolOutput::error(format!("unknown tool: {name}")))
        }
        .instrument(span)
        .await
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|t| t.definition()).collect()
    }
}

#[async_trait]
impl ToolExecutor for CompositeExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        for executor in &self.executors {
            if executor.tools().iter().any(|tool| tool.name == name) {
                return executor.execute(name, arguments, ctx).await;
            }
        }
        Ok(ToolOutput::error(format!("unknown tool: {name}")))
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.executors.iter().flat_map(|e| e.tools()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{FakeTool, SimpleExecutor};
    use crate::types::TrustLevel;
    use std::path::PathBuf;

    fn tool_context() -> ToolContext {
        ToolContext {
            session_id: "session".to_string(),
            trust: TrustLevel::Full,
            workspace: PathBuf::from("."),
        }
    }

    #[tokio::test]
    async fn composite_executor_routes_to_matching_executor() {
        let mut first = SimpleExecutor::new();
        first.add(Box::new(FakeTool::new("first_tool", "first output")));

        let mut second = SimpleExecutor::new();
        second.add(Box::new(FakeTool::new("second_tool", "second output")));

        let executor = CompositeExecutor::new(vec![Box::new(first), Box::new(second)]);

        let first_output = executor
            .execute("first_tool", serde_json::json!({}), &tool_context())
            .await
            .unwrap();
        assert_eq!(first_output.content, "first output");

        let second_output = executor
            .execute("second_tool", serde_json::json!({}), &tool_context())
            .await
            .unwrap();
        assert_eq!(second_output.content, "second output");
    }

    #[tokio::test]
    async fn composite_executor_returns_unknown_for_missing_tool() {
        let executor = CompositeExecutor::new(Vec::new());
        let output = executor
            .execute("missing", serde_json::json!({}), &tool_context())
            .await
            .unwrap();

        assert!(output.is_error);
        assert_eq!(output.content, "unknown tool: missing");
    }
}
