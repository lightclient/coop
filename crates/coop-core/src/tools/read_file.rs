use crate::tool_args::reject_unknown_fields;
use crate::tools::truncate;
use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use tracing::debug;

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

fn resolve_path(ctx: &ToolContext, path_str: &str) -> Result<PathBuf, ToolOutput> {
    ctx.workspace_scope
        .resolve_user_path_for_read(path_str)
        .map_err(|error| ToolOutput::error(error.to_string()))
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "read_file",
            "Read a file (relative to workspace). Truncated to 2000 lines or 50KB. Use offset/limit for large files; continue with offset until complete.",
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if let Some(output) =
            reject_unknown_fields("read_file", &arguments, &["path", "offset", "limit"])
        {
            return Ok(output);
        }

        let path_str = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: path"))?;

        let resolved = match resolve_path(ctx, path_str) {
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

        let r = truncate::truncate_head(&output);
        if r.was_truncated {
            let kept_lines = r.output.lines().count();
            let notice = format!(
                "{content}\n[Showing first {kept} of {total} lines (50KB limit). Use offset={next} to continue.]",
                content = r.output,
                kept = kept_lines,
                total = r.total_lines,
                next = kept_lines + 1,
            );
            Ok(ToolOutput::success(notice))
        } else {
            Ok(ToolOutput::success(r.output))
        }
    }
}

// Re-export resolve_path for write_file to use
pub(crate) fn resolve_workspace_path(
    ctx: &ToolContext,
    path_str: &str,
) -> Result<PathBuf, ToolOutput> {
    ctx.workspace_scope
        .resolve_user_path_for_write(path_str)
        .map_err(|error| ToolOutput::error(error.to_string()))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::ToolContext;
    use crate::types::TrustLevel;
    use crate::{SessionKind, group_workspace_dir_name};

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new("test", SessionKind::Main, TrustLevel::Full, dir, None)
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
    async fn reject_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = ReadFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "hello.txt", "unexpected": true}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("unknown field"));
        assert!(output.content.contains("unexpected"));
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

    #[tokio::test]
    async fn full_trust_non_group_can_read_other_user_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("users/bob")).unwrap();
        std::fs::write(dir.path().join("users/bob/note.txt"), "hello bob").unwrap();

        let ctx = ToolContext::new(
            "test",
            SessionKind::Dm("signal:alice-uuid".to_owned()),
            TrustLevel::Full,
            dir.path(),
            Some("alice"),
        );
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "users/bob/note.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content, "hello bob");
    }

    #[tokio::test]
    async fn inner_trust_user_reads_from_own_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("users/bob")).unwrap();
        std::fs::write(dir.path().join("users/bob/note.txt"), "hello bob").unwrap();

        let ctx = ToolContext::new(
            "test",
            SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            dir.path(),
            Some("bob"),
        );
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "note.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content, "hello bob");
    }

    #[tokio::test]
    async fn group_session_cannot_escape_to_user_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("users/alice")).unwrap();
        std::fs::write(dir.path().join("users/alice/secret.txt"), "secret").unwrap();

        let ctx = ToolContext::new(
            "test",
            SessionKind::Group("signal:group:deadbeef".to_owned()),
            TrustLevel::Owner,
            dir.path(),
            Some("alice"),
        );
        let tool = ReadFileTool;

        let output = tool
            .execute(
                serde_json::json!({"path": "../users/alice/secret.txt"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("path traversal"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("users/bob")).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), dir.path().join("users/bob/link")).unwrap();

        let ctx = ToolContext::new(
            "test",
            SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            dir.path(),
            Some("bob"),
        );
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "link/secret.txt"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(
            output
                .content
                .contains("outside the current workspace scope")
        );
    }

    #[tokio::test]
    async fn group_session_reads_from_group_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let group_dir = group_workspace_dir_name("signal:group:deadbeef");
        std::fs::create_dir_all(dir.path().join("groups").join(&group_dir)).unwrap();
        std::fs::write(
            dir.path().join("groups").join(&group_dir).join("note.txt"),
            "group note",
        )
        .unwrap();

        let ctx = ToolContext::new(
            "test",
            SessionKind::Group("signal:group:deadbeef".to_owned()),
            TrustLevel::Full,
            dir.path(),
            Some("alice"),
        );
        let tool = ReadFileTool;

        let output = tool
            .execute(serde_json::json!({"path": "note.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content, "group note");
    }
}
