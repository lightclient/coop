//! Line-aware output truncation for tool results.
//!
//! Two strategies: head (keep beginning, for file reads) and tail (keep end,
//! for bash output where errors appear last). Both respect line boundaries
//! and enforce both a line count and byte size limit.

use std::io::Write;
use tempfile::NamedTempFile;

const MAX_LINES: usize = 2000;
const MAX_BYTES: usize = 50_000;

#[derive(Debug)]
pub struct TruncateResult {
    pub output: String,
    pub was_truncated: bool,
    pub total_lines: usize,
}

/// Truncate keeping the HEAD (beginning) of output.
///
/// Used for file reads — the start of a file is most relevant.
/// Never breaks mid-line.
pub fn truncate_head(input: &str) -> TruncateResult {
    let lines: Vec<&str> = input.lines().collect();
    let total_lines = lines.len();

    if input.len() <= MAX_BYTES && total_lines <= MAX_LINES {
        return TruncateResult {
            output: input.to_owned(),
            was_truncated: false,
            total_lines,
        };
    }

    let mut byte_count = 0;
    let mut kept = 0;

    for line in &lines {
        let line_bytes = line.len() + 1; // +1 for newline
        if kept >= MAX_LINES || byte_count + line_bytes > MAX_BYTES {
            break;
        }
        byte_count += line_bytes;
        kept += 1;
    }

    // Ensure we keep at least one line even if it alone exceeds MAX_BYTES
    if kept == 0 && !lines.is_empty() {
        kept = 1;
    }

    let output = lines[..kept].join("\n");
    TruncateResult {
        output,
        was_truncated: true,
        total_lines,
    }
}

/// Truncate keeping the TAIL (end) of output.
///
/// Used for bash — errors and final state appear at the end.
/// Never breaks mid-line.
pub fn truncate_tail(input: &str) -> TruncateResult {
    let lines: Vec<&str> = input.lines().collect();
    let total_lines = lines.len();

    if input.len() <= MAX_BYTES && total_lines <= MAX_LINES {
        return TruncateResult {
            output: input.to_owned(),
            was_truncated: false,
            total_lines,
        };
    }

    let mut byte_count = 0;
    let mut kept = 0;

    for line in lines.iter().rev() {
        let line_bytes = line.len() + 1;
        if kept >= MAX_LINES || byte_count + line_bytes > MAX_BYTES {
            break;
        }
        byte_count += line_bytes;
        kept += 1;
    }

    // Ensure we keep at least one line
    if kept == 0 && !lines.is_empty() {
        kept = 1;
    }

    let start = total_lines - kept;
    let output = lines[start..].join("\n");
    TruncateResult {
        output,
        was_truncated: true,
        total_lines,
    }
}

/// Write full output to a temp file and return the path.
///
/// The file is persisted (not auto-deleted) so the model can reference it.
pub fn spill_to_temp_file(content: &str) -> Option<String> {
    let mut file = NamedTempFile::with_prefix("coop-output-").ok()?;
    file.write_all(content.as_bytes()).ok()?;
    let path = file.into_temp_path();
    // Persist so the file isn't deleted when the TempPath drops
    let persisted = path.keep().ok()?;
    Some(persisted.to_string_lossy().into_owned())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let r = truncate_head("");
        assert!(!r.was_truncated);
        assert_eq!(r.output, "");
        assert_eq!(r.total_lines, 0);

        let r = truncate_tail("");
        assert!(!r.was_truncated);
        assert_eq!(r.output, "");
        assert_eq!(r.total_lines, 0);
    }

    #[test]
    fn single_short_line_unchanged() {
        let input = "hello world";
        let r = truncate_head(input);
        assert!(!r.was_truncated);
        assert_eq!(r.output, input);

        let r = truncate_tail(input);
        assert!(!r.was_truncated);
        assert_eq!(r.output, input);
    }

    #[test]
    fn head_keeps_first_n_lines_when_line_limit_triggers() {
        let lines: Vec<String> = (0..3000).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");

        let r = truncate_head(&input);
        assert!(r.was_truncated);
        assert_eq!(r.total_lines, 3000);

        let kept: Vec<&str> = r.output.lines().collect();
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0], "line 0");
        assert_eq!(kept[MAX_LINES - 1], format!("line {}", MAX_LINES - 1));
    }

    #[test]
    fn tail_keeps_last_n_lines_when_line_limit_triggers() {
        let lines: Vec<String> = (0..3000).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");

        let r = truncate_tail(&input);
        assert!(r.was_truncated);
        assert_eq!(r.total_lines, 3000);

        let kept: Vec<&str> = r.output.lines().collect();
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0], format!("line {}", 3000 - MAX_LINES));
        assert_eq!(kept[MAX_LINES - 1], "line 2999");
    }

    #[test]
    fn byte_limit_triggers_before_line_limit() {
        // Each line is ~100 bytes. At 50KB limit, we should get ~500 lines
        // which is well under the 2000 line limit.
        let lines: Vec<String> = (0..1000)
            .map(|i| format!("line {:04} {}", i, "x".repeat(90)))
            .collect();
        let input = lines.join("\n");

        let r = truncate_head(&input);
        assert!(r.was_truncated);
        assert_eq!(r.total_lines, 1000);

        let kept: Vec<&str> = r.output.lines().collect();
        assert!(kept.len() < MAX_LINES);
        assert!(r.output.len() <= MAX_BYTES + 200); // some slack for the join
    }

    #[test]
    fn tail_byte_limit_triggers_before_line_limit() {
        let lines: Vec<String> = (0..1000)
            .map(|i| format!("line {:04} {}", i, "x".repeat(90)))
            .collect();
        let input = lines.join("\n");

        let r = truncate_tail(&input);
        assert!(r.was_truncated);

        let kept: Vec<&str> = r.output.lines().collect();
        assert!(kept.len() < MAX_LINES);
        // Last line should be "line 0999 ..."
        assert!(kept.last().unwrap().starts_with("line 0999"));
    }

    #[test]
    fn multibyte_utf8_never_broken() {
        // Lines with multi-byte chars
        let lines: Vec<String> = (0..3000).map(|i| format!("🦀 line {i} 🦀")).collect();
        let input = lines.join("\n");

        let r = truncate_head(&input);
        assert!(r.was_truncated);
        // Should be valid UTF-8 — would panic at .lines() if not
        let _ = r.output.lines().count();

        let r = truncate_tail(&input);
        assert!(r.was_truncated);
        let _ = r.output.lines().count();
    }

    #[test]
    fn under_both_limits_returns_unchanged() {
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");

        let r = truncate_head(&input);
        assert!(!r.was_truncated);
        assert_eq!(r.output, input);
        assert_eq!(r.total_lines, 10);

        let r = truncate_tail(&input);
        assert!(!r.was_truncated);
        assert_eq!(r.output, input);
    }

    #[test]
    fn spill_to_temp_file_works() {
        let content = "hello\nworld\n";
        let path = spill_to_temp_file(content).expect("should create temp file");
        let read_back = std::fs::read_to_string(&path).expect("should read back");
        assert_eq!(read_back, content);
        std::fs::remove_file(&path).ok();
    }
}
