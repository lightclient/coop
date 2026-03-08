use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput, TrustLevel};
use anyhow::Result;
use async_trait::async_trait;
use tracing::debug;

#[derive(Debug)]
pub struct WriteFileTool;

impl WriteFileTool {
    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "write_file",
            "Write content to a file, creating it if it doesn't exist",
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "write_file tool requires Full or Inner trust level",
            ));
        }

        let path_str = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: path"))?;

        let content = arguments
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: content"))?;

        let resolved = match super::read_file::resolve_workspace_path(ctx, path_str) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };

        // Create parent directories
        if let Some(parent) = resolved.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(ToolOutput::error(format!(
                "failed to create directories: {e}"
            )));
        }

        let bytes = content.len();
        match tokio::fs::write(&resolved, content).await {
            Ok(()) => {
                debug!(path = %path_str, bytes_written = bytes, "write_file complete");
                Ok(ToolOutput::success(format!(
                    "wrote {bytes} bytes to {path_str}"
                )))
            }
            Err(e) => Ok(ToolOutput::error(format!("failed to write file: {e}"))),
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::ToolContext;
    use crate::types::TrustLevel;
    use crate::{SessionKind, group_workspace_dir_name};
    use std::path::PathBuf;

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new("test", SessionKind::Main, TrustLevel::Full, dir, None)
    }

    #[tokio::test]
    async fn write_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = WriteFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "test.txt", "content": "hello"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("5 bytes"));
        let written = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn write_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = WriteFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "sub/dir/test.txt", "content": "nested"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        let written = std::fs::read_to_string(dir.path().join("sub/dir/test.txt")).unwrap();
        assert_eq!(written, "nested");
    }

    #[tokio::test]
    async fn trust_gate() {
        let ctx = ToolContext::new(
            "test",
            SessionKind::Main,
            TrustLevel::Familiar,
            PathBuf::from("/tmp"),
            None,
        );
        let tool = WriteFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "test.txt", "content": "nope"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("trust level"));
    }

    #[tokio::test]
    async fn reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = WriteFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "/etc/evil.txt", "content": "bad"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("absolute"));
    }

    #[tokio::test]
    async fn inner_trust_user_writes_inside_own_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(
            "test",
            SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            dir.path(),
            Some("bob"),
        );
        let tool = WriteFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "note.txt", "content": "bob note"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("users/bob/note.txt")).unwrap(),
            "bob note"
        );
    }

    #[tokio::test]
    async fn group_session_writes_inside_group_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let group_dir = group_workspace_dir_name("signal:group:deadbeef");
        let ctx = ToolContext::new(
            "test",
            SessionKind::Group("signal:group:deadbeef".to_owned()),
            TrustLevel::Full,
            dir.path(),
            Some("alice"),
        );
        let tool = WriteFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "note.txt", "content": "group note"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("groups").join(group_dir).join("note.txt"))
                .unwrap(),
            "group note"
        );
    }
}
