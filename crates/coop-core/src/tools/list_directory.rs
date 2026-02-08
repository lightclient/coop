use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use tracing::debug;

#[derive(Debug)]
pub struct ListDirectoryTool;

impl ListDirectoryTool {
    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to workspace (defaults to workspace root)"
                }
            }
        })
    }
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "list_directory",
            "List files and directories at a path",
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");

        let target = if path_str == "." {
            ctx.workspace.clone()
        } else {
            let path = PathBuf::from(path_str);
            if path.is_absolute() {
                return Ok(ToolOutput::error(
                    "absolute paths are not allowed; use paths relative to workspace",
                ));
            }
            let resolved = ctx.workspace.join(&path);

            // Verify inside workspace
            if resolved.exists() {
                let canon_workspace = ctx
                    .workspace
                    .canonicalize()
                    .map_err(|e| anyhow::anyhow!("workspace path error: {e}"))?;
                let canon_resolved = resolved
                    .canonicalize()
                    .map_err(|e| anyhow::anyhow!("path error: {e}"))?;
                if !canon_resolved.starts_with(&canon_workspace) {
                    return Ok(ToolOutput::error(
                        "path traversal outside workspace is not allowed",
                    ));
                }
            }
            resolved
        };

        let mut read_dir = match tokio::fs::read_dir(&target).await {
            Ok(rd) => rd,
            Err(e) => return Ok(ToolOutput::error(format!("failed to read directory: {e}"))),
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata().await;

            match meta {
                Ok(m) if m.is_dir() => dirs.push(format!("  {name}/")),
                Ok(m) if m.is_symlink() => files.push(format!("  {name} -> (symlink)")),
                Ok(m) => {
                    let size = m.len();
                    files.push(format!("  {name} ({size} bytes)"));
                }
                Err(_) => files.push(format!("  {name} (unknown)")),
            }
        }

        dirs.sort();
        files.sort();
        debug!(path = %path_str, entry_count = dirs.len() + files.len(), "list_directory complete");

        let mut output = Vec::new();
        if !dirs.is_empty() {
            output.push("Directories:".to_owned());
            output.extend(dirs);
        }
        if !files.is_empty() {
            output.push("Files:".to_owned());
            output.extend(files);
        }

        if output.is_empty() {
            Ok(ToolOutput::success("(empty directory)"))
        } else {
            Ok(ToolOutput::success(output.join("\n")))
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::ToolContext;
    use crate::types::TrustLevel;

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            trust: TrustLevel::Full,
            workspace: dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn list_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let ctx = test_ctx(dir.path());
        let tool = ListDirectoryTool;

        let output = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("subdir/"));
        assert!(output.content.contains("file.txt"));
    }

    #[tokio::test]
    async fn list_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/inner.txt"), "x").unwrap();

        let ctx = test_ctx(dir.path());
        let tool = ListDirectoryTool;

        let output = tool
            .execute(serde_json::json!({"path": "sub"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("inner.txt"));
    }

    #[tokio::test]
    async fn list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ListDirectoryTool;

        let output = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("empty"));
    }

    #[tokio::test]
    async fn reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ListDirectoryTool;

        let output = tool
            .execute(serde_json::json!({"path": "/etc"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("absolute"));
    }
}
