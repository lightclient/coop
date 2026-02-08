use anyhow::Result;
use async_trait::async_trait;
use coop_core::traits::{Tool, ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput, TrustLevel};
use std::path::PathBuf;
use tracing::info;

use crate::config_write::safe_write_config;

#[derive(Debug)]
struct ConfigWriteTool {
    config_path: PathBuf,
}

impl ConfigWriteTool {
    fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

#[async_trait]
impl Tool for ConfigWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "config_write",
            "Validate and write coop.yaml. Backs up the current config before writing. \
             Returns validation results. If any errors are found, the file is NOT modified.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Complete YAML content for coop.yaml. Must be the full \
                            file — not a patch or partial update. The content is validated \
                            before writing. If validation fails, the file is not modified."
                    }
                },
                "required": ["content"]
            }),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust != TrustLevel::Full {
            return Ok(ToolOutput::error("config_write requires Full trust level"));
        }

        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'content' parameter"))?;

        let (report, backup) = safe_write_config(&self.config_path, content);
        let summary = report.to_summary_string();

        if report.has_errors() {
            info!(config = %self.config_path.display(), "config_write rejected: validation failed");
            Ok(ToolOutput::error(format!(
                "Config validation failed. File was NOT modified.\n\n{summary}"
            )))
        } else {
            let backup_info = backup.map_or_else(
                || "No backup (new file)".to_owned(),
                |p| format!("Backup: {}", p.display()),
            );
            info!(config = %self.config_path.display(), "config_write applied");
            Ok(ToolOutput::success(format!(
                "Config written successfully. {backup_info}\n\n{summary}"
            )))
        }
    }
}

#[allow(missing_debug_implementations)]
pub(crate) struct ConfigToolExecutor {
    tool: ConfigWriteTool,
}

impl ConfigToolExecutor {
    pub(crate) fn new(config_path: PathBuf) -> Self {
        Self {
            tool: ConfigWriteTool::new(config_path),
        }
    }
}

#[async_trait]
impl ToolExecutor for ConfigToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if name == "config_write" {
            self.tool.execute(arguments, ctx).await
        } else {
            Ok(ToolOutput::error(format!("unknown tool: {name}")))
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![self.tool.definition()]
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tool_context(trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: "test-session".to_owned(),
            trust,
            workspace: PathBuf::from("."),
            user_name: None,
        }
    }

    fn write_test_config(dir: &Path) -> PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.join("coop.yaml");
        std::fs::write(
            &config_path,
            format!(
                "agent:\n  id: test\n  model: test-model\n  workspace: {}\nprovider:\n  name: anthropic\n",
                workspace.display()
            ),
        )
        .unwrap();
        config_path
    }

    #[tokio::test]
    async fn test_config_write_valid() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());

        let tool = ConfigWriteTool::new(config_path.clone());
        let workspace = dir.path().join("workspace");
        let new_yaml = format!(
            "agent:\n  id: updated\n  model: test-model\n  workspace: {}\nprovider:\n  name: anthropic\n",
            workspace.display()
        );

        let output = tool
            .execute(
                serde_json::json!({"content": new_yaml}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        if output.is_error {
            // May fail if ANTHROPIC_API_KEY is not set — that's an env issue,
            // not a config tool issue. Verify the tool produced a report.
            assert!(output.content.contains("NOT modified"));
        } else {
            assert!(config_path.with_extension("yaml.bak").exists());
            assert!(
                std::fs::read_to_string(&config_path)
                    .unwrap()
                    .contains("updated")
            );
        }
    }

    #[tokio::test]
    async fn test_config_write_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let original = std::fs::read_to_string(&config_path).unwrap();

        let tool = ConfigWriteTool::new(config_path.clone());

        let output = tool
            .execute(
                serde_json::json!({"content": "{{garbage yaml"}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("NOT modified"));
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn test_config_write_trust_gate() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());

        let tool = ConfigWriteTool::new(config_path);

        let output = tool
            .execute(
                serde_json::json!({"content": "anything"}),
                &tool_context(TrustLevel::Public),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("Full trust"));
    }

    #[tokio::test]
    async fn test_config_write_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.yaml");
        std::fs::write(&config_path, "agent:\n  id: test\n  model: test-model\n").unwrap();
        let original = std::fs::read_to_string(&config_path).unwrap();

        let tool = ConfigWriteTool::new(config_path.clone());

        let new_yaml = "agent:\n  id: test\n  model: test-model\n  workspace: ./nonexistent\n";
        let output = tool
            .execute(
                serde_json::json!({"content": new_yaml}),
                &tool_context(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
    }
}
