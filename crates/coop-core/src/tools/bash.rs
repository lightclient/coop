use crate::tools::truncate;
use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput, TrustLevel};
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct BashTool;

impl BashTool {
    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        })
    }
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "bash",
            "Execute a shell command and return stdout/stderr",
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "bash tool requires Full or Inner trust level",
            ));
        }

        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: command"))?;

        let result = tokio::time::timeout(
            TIMEOUT,
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&ctx.workspace)
                .output(),
        )
        .await;

        match result {
            Err(_) => Ok(ToolOutput::error(format!(
                "command timed out after {}s",
                TIMEOUT.as_secs()
            ))),
            Ok(Err(e)) => Ok(ToolOutput::error(format!("failed to execute command: {e}"))),
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                debug!(
                    exit_code = output.status.code().unwrap_or(-1),
                    stdout_len = stdout.len(),
                    stderr_len = stderr.len(),
                    "bash complete"
                );

                let mut combined = stdout.into_owned();
                if !stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&stderr);
                }

                let r = truncate::truncate_tail(&combined);
                let final_output = if r.was_truncated {
                    let temp_note = truncate::spill_to_temp_file(&combined)
                        .map(|p| format!(" Full output: {p}"))
                        .unwrap_or_default();
                    let kept_lines = r.output.lines().count();
                    format!(
                        "... [output truncated: {total} lines, showing last {kept}.{temp_note}]\n{content}",
                        total = r.total_lines,
                        kept = kept_lines,
                        content = r.output,
                    )
                } else {
                    r.output
                };

                if output.status.success() {
                    if final_output.is_empty() {
                        Ok(ToolOutput::success("(no output)"))
                    } else {
                        Ok(ToolOutput::success(final_output))
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    Ok(ToolOutput::error(format!(
                        "exit code {code}\n{final_output}"
                    )))
                }
            }
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            trust: TrustLevel::Full,
            workspace: dir.to_path_buf(),
            user_name: None,
        }
    }

    #[tokio::test]
    async fn echo_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        let output = tool
            .execute(serde_json::json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content.trim(), "hello");
    }

    #[tokio::test]
    async fn failing_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        let output = tool
            .execute(serde_json::json!({"command": "exit 1"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("exit code 1"));
    }

    #[tokio::test]
    async fn trust_gate() {
        let ctx = ToolContext {
            session_id: "test".into(),
            trust: TrustLevel::Public,
            workspace: PathBuf::from("/tmp"),
            user_name: None,
        };
        let tool = BashTool;

        let output = tool
            .execute(serde_json::json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("trust level"));
    }

    #[tokio::test]
    async fn truncation_uses_tail_strategy() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        // Generate 3000 lines — exceeds the 2000 line limit.
        let cmd = "seq 1 3000";
        let output = tool
            .execute(serde_json::json!({"command": cmd}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("[output truncated"));
        assert!(output.content.contains("showing last"));
        // Tail strategy: should contain the last line, not the first.
        assert!(output.content.contains("3000"));
        // First line should be truncated away
        assert!(!output.content.starts_with("1\n"));
    }

    #[tokio::test]
    async fn uses_workspace_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "found").unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        let output = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(output.content.trim(), "found");
    }
}
