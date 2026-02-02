use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_core::traits::{Provider, ProviderStream};
use coop_core::types::{Message, ModelInfo, Role, ToolDef, Usage};
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, error, warn};

/// Goose subprocess-based agent runtime (fallback).
#[derive(Debug)]
pub struct GooseRuntime {
    pub model: String,
    pub provider: String,
    pub goose_bin: String,
    api_key: String,
    model_info: ModelInfo,
}

impl GooseRuntime {
    pub fn new(model: impl Into<String>, provider: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY must be set in the environment")?;

        let model = model.into();
        let model_info = ModelInfo {
            name: model.clone(),
            context_limit: 128_000,
        };
        Ok(Self {
            model,
            provider: provider.into(),
            goose_bin: "/opt/homebrew/bin/goose".to_string(),
            api_key,
            model_info,
        })
    }

    /// Build the last user message from the message history.
    fn extract_user_message(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(Message::text)
            .unwrap_or_default()
    }

    /// Parse streaming JSON output from goose.
    /// Each line is a JSON object. We look for content in various formats.
    fn parse_output(raw: &str) -> String {
        let mut content_parts: Vec<String> = Vec::new();

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Try to parse as JSON
            if let Ok(val) = serde_json::from_str::<Value>(line) {
                // Look for content in common patterns
                if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                    content_parts.push(content.to_string());
                } else if let Some(text) = val.get("text").and_then(|t| t.as_str()) {
                    content_parts.push(text.to_string());
                } else if let Some(delta) = val.get("delta")
                    && let Some(text) = delta.get("text").and_then(|t| t.as_str())
                {
                    content_parts.push(text.to_string());
                }
                // Also check for message.content pattern
                if let Some(msg) = val.get("message")
                    && let Some(content) = msg.get("content").and_then(|c| c.as_str())
                    && !content_parts.contains(&content.to_string())
                {
                    content_parts.push(content.to_string());
                }
            } else {
                // Not JSON — could be plain text output from goose
                debug!("non-JSON goose output: {}", line);
            }
        }

        if content_parts.is_empty() {
            // Fall back to raw output, stripping obvious JSON noise
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .filter(|l| serde_json::from_str::<Value>(l).is_err())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        } else {
            content_parts.join("")
        }
    }
}

#[async_trait]
impl Provider for GooseRuntime {
    fn name(&self) -> &str {
        &self.provider
    }

    fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let user_message = Self::extract_user_message(messages);
        if user_message.is_empty() {
            anyhow::bail!("no user message found in conversation history");
        }

        debug!(
            model = %self.model,
            provider = %self.provider,
            message_len = user_message.len(),
            "spawning goose subprocess"
        );

        let mut cmd = Command::new(&self.goose_bin);
        cmd.arg("run")
            .arg("-t")
            .arg(&user_message)
            .arg("--system")
            .arg(system)
            .arg("--no-session")
            .arg("--quiet")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--max-turns")
            .arg("10")
            .arg("--provider")
            .arg(&self.provider)
            .arg("--model")
            .arg(&self.model)
            .arg("--with-builtin")
            .arg("developer")
            .env("ANTHROPIC_API_KEY", &self.api_key)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("failed to spawn goose process")?;

        // Read stdout
        let stdout = child.stdout.take().context("failed to capture stdout")?;
        let stderr = child.stderr.take().context("failed to capture stderr")?;

        let stdout_handle = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut output = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                output.push_str(&line);
                output.push('\n');
            }
            output
        });

        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            let mut output = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                output.push_str(&line);
                output.push('\n');
            }
            output
        });

        let status = child.wait().await.context("failed to wait for goose")?;
        let raw_stdout = stdout_handle.await.unwrap_or_default();
        let raw_stderr = stderr_handle.await.unwrap_or_default();

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            error!(
                exit_code = code,
                stderr = %raw_stderr.trim(),
                "goose exited with error"
            );
            anyhow::bail!("goose exited with code {}: {}", code, raw_stderr.trim());
        }

        if !raw_stderr.trim().is_empty() {
            warn!(stderr = %raw_stderr.trim(), "goose stderr output");
        }

        let content = Self::parse_output(&raw_stdout);
        if content.is_empty() {
            warn!(
                "goose produced no parseable content, raw stdout len={}",
                raw_stdout.len()
            );
        }

        Ok((
            Message::assistant().with_text(content),
            Usage::default(),
        ))
    }

    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        anyhow::bail!("GooseRuntime subprocess does not support streaming")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_content() {
        let raw = r#"{"content":"Hello world"}
{"content":" How are you?"}
"#;
        let result = GooseRuntime::parse_output(raw);
        assert_eq!(result, "Hello world How are you?");
    }

    #[test]
    fn test_parse_text_field() {
        let raw = r#"{"text":"Some text"}
"#;
        let result = GooseRuntime::parse_output(raw);
        assert_eq!(result, "Some text");
    }

    #[test]
    fn test_parse_plain_text_fallback() {
        let raw = "Just plain text output\nMore text\n";
        let result = GooseRuntime::parse_output(raw);
        assert_eq!(result, "Just plain text output\nMore text");
    }

    #[test]
    fn test_extract_user_message() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_text("hi there"),
            Message::user().with_text("how are you"),
        ];
        let result = GooseRuntime::extract_user_message(&messages);
        assert_eq!(result, "how are you");
    }
}
