use crate::traits::{Tool, ToolContext};
use crate::types::{ToolDef, ToolOutput, TrustLevel};
use anyhow::Result;
use async_trait::async_trait;
use tracing::debug;

const DIFF_CONTEXT_LINES: usize = 4;

#[derive(Debug)]
pub struct EditFileTool;

impl EditFileTool {
    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace"
                },
                "oldText": {
                    "type": "string",
                    "description": "Exact text to find and replace (must match exactly)"
                },
                "newText": {
                    "type": "string",
                    "description": "New text to replace the old text with"
                }
            },
            "required": ["path", "oldText", "newText"]
        })
    }
}

#[derive(Debug)]
struct MatchResult {
    index: usize,
    match_length: usize,
    content_for_replacement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn normalize_for_fuzzy_match(text: &str) -> String {
    let trimmed_lines = text
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");

    trimmed_lines
        .chars()
        .map(|ch| match ch {
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            _ => ch,
        })
        .collect()
}

fn fuzzy_find_text(content: &str, old_text: &str) -> Option<MatchResult> {
    if let Some(index) = content.find(old_text) {
        return Some(MatchResult {
            index,
            match_length: old_text.len(),
            content_for_replacement: content.to_owned(),
        });
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    let index = fuzzy_content.find(&fuzzy_old_text)?;

    Some(MatchResult {
        index,
        match_length: fuzzy_old_text.len(),
        content_for_replacement: fuzzy_content,
    })
}

fn generate_diff(old_content: &str, new_content: &str, context_lines: usize) -> String {
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();

    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_change_end = old_lines.len().saturating_sub(suffix);
    let new_change_end = new_lines.len().saturating_sub(suffix);

    let leading_context_start = prefix.saturating_sub(context_lines);
    let trailing_context_count = context_lines.min(suffix);
    let trailing_context_start = new_lines.len().saturating_sub(suffix);

    let max_line_num = old_lines.len().max(new_lines.len()).max(1);
    let width = max_line_num.to_string().len();

    let mut output = Vec::new();

    if leading_context_start > 0 {
        output.push(" ... ".to_owned());
    }

    for (offset, line) in old_lines[leading_context_start..prefix].iter().enumerate() {
        let line_num = leading_context_start + offset + 1;
        output.push(format!(" {line_num:>width$} {line}"));
    }

    for (offset, line) in old_lines[prefix..old_change_end].iter().enumerate() {
        let line_num = prefix + offset + 1;
        output.push(format!("-{line_num:>width$} {line}"));
    }

    for (offset, line) in new_lines[prefix..new_change_end].iter().enumerate() {
        let line_num = prefix + offset + 1;
        output.push(format!("+{line_num:>width$} {line}"));
    }

    let trailing_context_end = trailing_context_start + trailing_context_count;
    for (offset, line) in new_lines[trailing_context_start..trailing_context_end]
        .iter()
        .enumerate()
    {
        let line_num = trailing_context_start + offset + 1;
        output.push(format!(" {line_num:>width$} {line}"));
    }

    if suffix > trailing_context_count {
        output.push(" ... ".to_owned());
    }

    output.join("\n")
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef::new(
            "edit_file",
            "Edit a file by replacing exact text. The oldText must match exactly (including whitespace). Use this for precise, surgical edits.",
            Self::schema(),
        )
    }

    async fn execute(&self, arguments: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "edit_file tool requires Full or Inner trust level",
            ));
        }

        let path_str = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: path"))?;

        let old_text = arguments
            .get("oldText")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: oldText"))?;

        let new_text = arguments
            .get("newText")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: newText"))?;

        let resolved = match super::read_file::resolve_workspace_path(&ctx.workspace, path_str) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };

        let raw_content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("failed to read file: {e}"))),
        };

        let (had_bom, content) = if let Some(stripped) = raw_content.strip_prefix('\u{FEFF}') {
            (true, stripped)
        } else {
            (false, raw_content.as_str())
        };

        let original_ending = if let (Some(crlf), Some(lf)) =
            (content.find("\r\n"), content.find('\n'))
            && crlf < lf
        {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };

        let normalized_content = normalize_to_lf(content);
        let normalized_old_text = normalize_to_lf(old_text);
        let normalized_new_text = normalize_to_lf(new_text);

        let Some(match_result) = fuzzy_find_text(&normalized_content, &normalized_old_text) else {
            return Ok(ToolOutput::error(format!(
                "Could not find the exact text in {path_str}. The old text must match exactly including all whitespace and newlines."
            )));
        };

        let fuzzy_content = normalize_for_fuzzy_match(&normalized_content);
        let fuzzy_old_text = normalize_for_fuzzy_match(&normalized_old_text);
        let occurrences = fuzzy_content.match_indices(&fuzzy_old_text).count();
        if occurrences > 1 {
            return Ok(ToolOutput::error(format!(
                "Found {occurrences} occurrences of the text in {path_str}. The text must be unique. Please provide more context to make it unique."
            )));
        }

        let base_content = match_result.content_for_replacement;
        let replacement_end = match_result.index + match_result.match_length;
        let mut new_content = String::with_capacity(
            base_content.len()
                + normalized_new_text
                    .len()
                    .saturating_sub(match_result.match_length),
        );
        new_content.push_str(&base_content[..match_result.index]);
        new_content.push_str(&normalized_new_text);
        new_content.push_str(&base_content[replacement_end..]);

        if base_content == new_content {
            return Ok(ToolOutput::error(format!(
                "No changes made to {path_str}. The replacement produced identical content."
            )));
        }

        let mut final_content = if original_ending == LineEnding::Crlf {
            new_content.replace('\n', "\r\n")
        } else {
            new_content.clone()
        };
        if had_bom {
            final_content.insert(0, '\u{FEFF}');
        }

        match tokio::fs::write(&resolved, &final_content).await {
            Ok(()) => {
                debug!(path = %path_str, bytes_written = final_content.len(), "edit_file complete");
                let diff = generate_diff(&base_content, &new_content, DIFF_CONTEXT_LINES);
                Ok(ToolOutput::success(format!(
                    "Successfully edited {path_str}\n{diff}"
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

    fn test_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            trust: TrustLevel::Full,
            workspace: dir.to_path_buf(),
            user_name: None,
        }
    }

    async fn run_edit(
        tool: &EditFileTool,
        ctx: &ToolContext,
        path: &str,
        old_text: &str,
        new_text: &str,
    ) -> ToolOutput {
        tool.execute(
            serde_json::json!({
                "path": path,
                "oldText": old_text,
                "newText": new_text
            }),
            ctx,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn basic_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "test.txt",
            "world",
            "coop",
        )
        .await;
        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).unwrap(),
            "hello coop"
        );
    }

    #[tokio::test]
    async fn edit_preserves_surrounding_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("middle.txt"), "before\nmiddle\nafter\n").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "middle.txt",
            "middle",
            "center",
        )
        .await;
        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("middle.txt")).unwrap(),
            "before\ncenter\nafter\n"
        );
    }

    #[tokio::test]
    async fn edit_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "missing.txt",
            "a",
            "b",
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("failed to read file"));
    }

    #[tokio::test]
    async fn edit_text_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "alpha\nbeta\n").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "file.txt",
            "gamma",
            "delta",
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("Could not find the exact text"));
    }

    #[tokio::test]
    async fn edit_multiple_occurrences() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dupe.txt"), "repeat\nrepeat\n").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "dupe.txt",
            "repeat",
            "once",
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("Found 2 occurrences"));
    }

    #[tokio::test]
    async fn edit_no_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("same.txt"), "same").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "same.txt",
            "same",
            "same",
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("No changes made"));
    }

    #[tokio::test]
    async fn trust_gate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();

        for trust in [TrustLevel::Familiar, TrustLevel::Public] {
            let ctx = ToolContext {
                session_id: "test".into(),
                trust,
                workspace: dir.path().to_path_buf(),
                user_name: None,
            };
            let output = run_edit(&EditFileTool, &ctx, "file.txt", "hello", "hi").await;
            assert!(output.is_error);
            assert!(output.content.contains("trust level"));
        }
    }

    #[tokio::test]
    async fn reject_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "/etc/passwd",
            "root",
            "user",
        )
        .await;
        assert!(output.is_error);
        assert!(output.content.contains("absolute"));
    }

    #[tokio::test]
    async fn fuzzy_match_trailing_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fuzzy.txt"), "alpha\nbeta   \ngamma\t \n").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "fuzzy.txt",
            "beta\ngamma",
            "BETA\ngamma",
        )
        .await;
        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fuzzy.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn preserves_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("crlf.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "crlf.txt",
            "two",
            "TWO",
        )
        .await;
        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("crlf.txt")).unwrap(),
            "one\r\nTWO\r\nthree\r\n"
        );
    }

    #[tokio::test]
    async fn output_contains_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("diff.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let output = run_edit(
            &EditFileTool,
            &test_ctx(dir.path()),
            "diff.txt",
            "beta",
            "BETA",
        )
        .await;
        assert!(!output.is_error);
        assert!(output.content.contains("Successfully edited diff.txt"));
        assert!(output.content.contains("-2 beta"));
        assert!(output.content.contains("+2 BETA"));
    }
}
