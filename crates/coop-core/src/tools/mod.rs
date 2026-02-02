pub mod bash;
pub mod list_directory;
pub mod read_file;
pub mod write_file;

use crate::traits::{Tool, ToolContext, ToolExecutor};
use crate::types::{ToolDef, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;

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

#[async_trait]
impl ToolExecutor for DefaultExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        for tool in &self.tools {
            if tool.definition().name == name {
                return tool.execute(arguments, ctx).await;
            }
        }
        Ok(ToolOutput::error(format!("unknown tool: {name}")))
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|t| t.definition()).collect()
    }
}
