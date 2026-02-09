use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use tracing::debug;

const MAX_OUTPUT_BYTES: usize = 100_000;

#[derive(Debug)]
pub struct ReadFileTool;

impl ReadFileTool {
    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace"
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number (0-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of lines to read"
                }
            },
            "required": ["path"]
        })
    }
}

fn resolve_path(workspace: &std::path::Path, path_str: &str) -> Result<PathBuf, ToolOutput> {
    let path = PathBuf::from(path_str);

    if path.is_absolute() {
        return Err(ToolOutput::error(
            "absolute paths are not allowed; use paths relative to workspace",
        ));
    }

    let resolved = workspace.join(&path);

    // Canonicalize both to check for traversal
    // The workspace must exist, the target may not yet
    let canon_workspace = workspace
        .canonicalize()
        .map_err(|e| ToolOutput::error(format!("workspace path error: {e}")))?;

    // For existence checks, canonicalize the resolved path
    let canon_resolved = resolved
        .canonicalize()
        .map_err(|e| ToolOutput::error(format!("path error: {e}")))?;

    if !canon_resolved.starts_with(&canon_workspace) {
        return Err(ToolOutput::error(
            "path traversal outside workspace is not allowed",
        ));
    }

    Ok(canon_resolved)
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "read_file",
            "Read the contents of a file relative to the workspace",
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: path"))?;

        let resolved = match resolve_path(&ctx.workspace, path_str) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => {
                debug!(path = %path_str, bytes_read = c.len(), "read_file complete");
                c
            }
            Err(e) => return Ok(ToolOutput::error(format!("failed to read file: {e}"))),
        };

        let offset = arguments
            .get("offset")
            .and_then(serde_json::Value::as_u64)
            .map(|v| usize::try_from(v).unwrap_or(usize::MAX));
        let limit = arguments
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|v| usize::try_from(v).unwrap_or(usize::MAX));

        let output = if offset.is_some() || limit.is_some() {
            let lines: Vec<&str> = content.lines().collect();
            let start = offset.unwrap_or(0);
            let end = limit.map_or(lines.len(), |l| (start + l).min(lines.len()));

            if start >= lines.len() {
                return Ok(ToolOutput::success(format!(
                    "(file has {} lines, offset {} is past end)",
                    lines.len(),
                    start
                )));
            }

            lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{line}", start + i + 1))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            content
        };

        if output.len() > MAX_OUTPUT_BYTES {
            let mut truncated = output;
            let boundary = truncated.floor_char_boundary(MAX_OUTPUT_BYTES);
            truncated.truncate(boundary);
            truncated.push_str("\n... [output truncated]");
            Ok(ToolOutput::success(truncated))
        } else {
            Ok(ToolOutput::success(output))
        }
    }
}

// Re-export resolve_path for write_file to use
pub(crate) fn resolve_workspace_path(
    workspace: &std::path::Path,
    path_str: &str,
) -> Result<PathBuf, ToolOutput> {
    let path = PathBuf::from(path_str);

    if path.is_absolute() {
        return Err(ToolOutput::error(
            "absolute paths are not allowed; use paths relative to workspace",
        ));
    }

    let resolved = workspace.join(&path);

    let canon_workspace = workspace
        .canonicalize()
        .map_err(|e| ToolOutput::error(format!("workspace path error: {e}")))?;

    // For write, the file may not exist yet — check the parent
    let parent = resolved
        .parent()
        .ok_or_else(|| ToolOutput::error("invalid path"))?;

    // If parent doesn't exist yet, that's OK — we'll create it.
    // But if it does exist, verify it's inside workspace.
    if parent.exists() {
        let canon_parent = parent
            .canonicalize()
            .map_err(|e| ToolOutput::error(format!("path error: {e}")))?;
        if !canon_parent.starts_with(&canon_workspace) {
            return Err(ToolOutput::error(
                "path traversal outside workspace is not allowed",
            ));
        }
    }

    Ok(resolved)
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
            user_name: None,
        }
    }

    #[tokio::test]
    async fn read_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "hello.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content, "hello world");
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lines.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ReadFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "lines.txt", "offset": 1, "limit": 2}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains('b'));
        assert!(output.content.contains('c'));
        assert!(!output.content.contains('a'));
        assert!(!output.content.contains('d'));
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "nope.txt"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
    }

    #[tokio::test]
    async fn reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "/etc/passwd"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("absolute"));
    }

    #[tokio::test]
    async fn reject_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        // Create a file outside the workspace to attempt traversal to
        let outer = dir.path().parent().unwrap();
        std::fs::write(outer.join("secret.txt"), "secret").ok();

        let ctx = test_ctx(dir.path());
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "../secret.txt"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
    }
}
