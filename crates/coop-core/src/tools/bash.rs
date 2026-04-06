use crate::tool_args::reject_unknown_fields;
use crate::tools::truncate;
use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput, TrustLevel};
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const TIMEOUT_FIELD: &str = "timeout";
const TIMEOUT_SECONDS_FIELD: &str = "timeout_seconds";

pub fn timeout_from_arguments(arguments: &serde_json::Value) -> Result<Duration> {
    if let Some(timeout) = arguments
        .get(TIMEOUT_FIELD)
        .filter(|value| !value.is_null())
    {
        return parse_timeout_value(TIMEOUT_FIELD, timeout);
    }

    if let Some(timeout) = arguments
        .get(TIMEOUT_SECONDS_FIELD)
        .filter(|value| !value.is_null())
    {
        return parse_timeout_value(TIMEOUT_SECONDS_FIELD, timeout);
    }

    Ok(DEFAULT_TIMEOUT)
}

fn parse_timeout_value(field: &str, value: &serde_json::Value) -> Result<Duration> {
    let timeout_seconds = value
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a positive integer"))?;

    Ok(Duration::from_secs(timeout_seconds))
}

#[derive(Debug)]
pub struct BashTool;

impl BashTool {
    fn schema() -> serde_json::Value {
        let timeout_description = format!(
            "Optional per-call timeout in seconds (defaults to {}s; `timeout_seconds` alias also accepted)",
            DEFAULT_TIMEOUT.as_secs()
        );

        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "description": timeout_description
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Alias for timeout"
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
            format!(
                "Execute a shell command and return stdout/stderr. Optional timeout overrides the {}s default.",
                DEFAULT_TIMEOUT.as_secs()
            ),
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "bash tool requires Full or Inner trust level",
            ));
        }

        if let Some(output) = reject_unknown_fields(
            "bash",
            &arguments,
            &["command", "timeout", "timeout_seconds"],
        ) {
            return Ok(output);
        }

        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: command"))?;

        if let Err(error) = ctx
            .workspace_scope
            .ensure_scope_root_exists()
            .and_then(|()| ctx.workspace_scope.scope_root().map(|_| ()))
        {
            return Ok(ToolOutput::error(error.to_string()));
        }

        let timeout = match timeout_from_arguments(&arguments) {
            Ok(timeout) => timeout,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };

        debug!(
            command_len = command.len(),
            timeout_seconds = timeout.as_secs(),
            "bash starting"
        );

        let result = tokio::time::timeout(
            timeout,
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
                timeout.as_secs()
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
    use crate::SessionKind;
    use std::path::PathBuf;

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new("test", SessionKind::Main, TrustLevel::Full, dir, None)
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

    #[test]
    fn timeout_defaults_to_120_seconds() {
        assert_eq!(
            timeout_from_arguments(&serde_json::json!({})).unwrap(),
            DEFAULT_TIMEOUT
        );
    }

    #[test]
    fn timeout_seconds_alias_is_supported() {
        assert_eq!(
            timeout_from_arguments(&serde_json::json!({"timeout_seconds": 7})).unwrap(),
            Duration::from_secs(7)
        );
    }

    #[tokio::test]
    async fn reject_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        let output = tool
            .execute(
                serde_json::json!({"command": "echo hello", "unexpected": true}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("unknown field"));
        assert!(output.content.contains("unexpected"));
    }

    #[test]
    fn timeout_rejects_non_positive_values() {
        assert!(timeout_from_arguments(&serde_json::json!({"timeout": 0})).is_err());
    }

    #[tokio::test]
    async fn custom_timeout_is_honored() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        let output = tool
            .execute(
                serde_json::json!({
                    "command": "sleep 2",
                    "timeout": 1
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("timed out after 1s"));
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
        let ctx = ToolContext::new(
            "test",
            SessionKind::Main,
            TrustLevel::Public,
            PathBuf::from("/tmp"),
            None,
        );
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

    #[tokio::test]
    async fn single_line_output_is_clipped_to_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let tool = BashTool;

        let output = tool
            .execute(
                serde_json::json!({
                    "command": "python3 - <<'PY'\nprint('x' * 120000)\nPY"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("[output truncated"));
        assert!(output.content.len() < 60_000);
        assert!(!output.content.contains(&"x".repeat(80_000)));
    }
}
