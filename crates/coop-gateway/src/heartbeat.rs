/// Token-based heartbeat suppression for cron delivery.
///
/// When the agent responds with only `HEARTBEAT_OK` (possibly wrapped in
/// markdown or whitespace), the response is suppressed and not delivered.
pub(crate) const HEARTBEAT_OK_TOKEN: &str = "HEARTBEAT_OK";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeartbeatResult {
    /// The response was only HEARTBEAT_OK (possibly wrapped in
    /// markdown/whitespace). Suppress delivery.
    Suppress,
    /// There is real content to deliver.
    Deliver(String),
}

/// Strip the `HEARTBEAT_OK` token from the edges of a response and decide
/// whether to suppress or deliver.
///
/// Rules:
/// - Empty / whitespace-only → Suppress
/// - Exact match or wrapped in markdown bold/italic → Suppress
/// - Token at start/end with real content → Deliver (token removed)
/// - Token mid-sentence → NOT stripped (only edges)
/// - No token, real content → Deliver as-is
pub(crate) fn strip_heartbeat_token(text: &str) -> HeartbeatResult {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return HeartbeatResult::Suppress;
    }

    // Unwrap markdown bold/italic then check for bare token.
    let unwrapped = unwrap_markdown(trimmed);
    if unwrapped == HEARTBEAT_OK_TOKEN {
        return HeartbeatResult::Suppress;
    }

    // Try stripping from start/end.
    let mut remainder = trimmed.to_owned();
    remainder = strip_token_from_start(&remainder);
    remainder = strip_token_from_end(&remainder);

    let cleaned = remainder.trim();
    if cleaned.is_empty() {
        HeartbeatResult::Suppress
    } else {
        HeartbeatResult::Deliver(cleaned.to_owned())
    }
}

/// Strip leading markdown (**, *, `) wrappers to expose the inner text.
fn unwrap_markdown(s: &str) -> &str {
    // Try ** first, then *, then `
    if let Some(inner) = s
        .strip_prefix("**")
        .and_then(|rest| rest.strip_suffix("**"))
    {
        return inner.trim();
    }
    if let Some(inner) = s.strip_prefix('*').and_then(|rest| rest.strip_suffix('*')) {
        return inner.trim();
    }
    if let Some(inner) = s.strip_prefix('`').and_then(|rest| rest.strip_suffix('`')) {
        return inner.trim();
    }
    s
}

/// Strip `HEARTBEAT_OK` from the beginning of the text, including any
/// immediately following punctuation and whitespace.
fn strip_token_from_start(text: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix(HEARTBEAT_OK_TOKEN) {
        // Also strip markdown-wrapped forms at start
        strip_leading_punctuation(rest).to_owned()
    } else if let Some(rest) = trimmed
        .strip_prefix("**")
        .and_then(|r| r.strip_prefix(HEARTBEAT_OK_TOKEN))
        .and_then(|r| r.strip_prefix("**"))
    {
        strip_leading_punctuation(rest).to_owned()
    } else if let Some(rest) = trimmed
        .strip_prefix('*')
        .and_then(|r| r.strip_prefix(HEARTBEAT_OK_TOKEN))
        .and_then(|r| r.strip_prefix('*'))
    {
        strip_leading_punctuation(rest).to_owned()
    } else if let Some(rest) = trimmed
        .strip_prefix('`')
        .and_then(|r| r.strip_prefix(HEARTBEAT_OK_TOKEN))
        .and_then(|r| r.strip_prefix('`'))
    {
        strip_leading_punctuation(rest).to_owned()
    } else {
        text.to_owned()
    }
}

/// Strip `HEARTBEAT_OK` from the end of the text, trimming only
/// whitespace between the content and the token.
fn strip_token_from_end(text: &str) -> String {
    let trimmed = text.trim_end();
    if let Some(rest) = trimmed.strip_suffix(HEARTBEAT_OK_TOKEN) {
        rest.trim_end().to_owned()
    } else if let Some(rest) = trimmed
        .strip_suffix("**")
        .and_then(|r| r.strip_suffix(HEARTBEAT_OK_TOKEN))
        .and_then(|r| r.strip_suffix("**"))
    {
        rest.trim_end().to_owned()
    } else if let Some(rest) = trimmed
        .strip_suffix('*')
        .and_then(|r| r.strip_suffix(HEARTBEAT_OK_TOKEN))
        .and_then(|r| r.strip_suffix('*'))
    {
        rest.trim_end().to_owned()
    } else if let Some(rest) = trimmed
        .strip_suffix('`')
        .and_then(|r| r.strip_suffix(HEARTBEAT_OK_TOKEN))
        .and_then(|r| r.strip_suffix('`'))
    {
        rest.trim_end().to_owned()
    } else {
        text.to_owned()
    }
}

fn strip_leading_punctuation(s: &str) -> &str {
    let s = s.trim_start_matches(['.', ',', ';', ':']);
    s.trim_start()
}

/// Returns true if a HEARTBEAT.md file contains only whitespace, comment
/// lines (lines starting with `#`), or empty list items (`- [ ]`, `- `).
pub(crate) fn is_heartbeat_content_empty(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Markdown header / comment lines
        if trimmed.starts_with('#') {
            continue;
        }
        // Empty list items: "- [ ]", "- [x]", "- "
        if trimmed == "-" || trimmed == "- [ ]" || trimmed == "- [x]" || trimmed == "- [X]" {
            continue;
        }
        // Line has real content
        return false;
    }
    true
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // -- strip_heartbeat_token tests --

    #[test]
    fn exact_match_suppresses() {
        assert_eq!(
            strip_heartbeat_token("HEARTBEAT_OK"),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn with_whitespace_suppresses() {
        assert_eq!(
            strip_heartbeat_token("  HEARTBEAT_OK  "),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn markdown_bold_suppresses() {
        assert_eq!(
            strip_heartbeat_token("**HEARTBEAT_OK**"),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn markdown_italic_suppresses() {
        assert_eq!(
            strip_heartbeat_token("*HEARTBEAT_OK*"),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn markdown_backtick_suppresses() {
        assert_eq!(
            strip_heartbeat_token("`HEARTBEAT_OK`"),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn backtick_token_at_start_with_content() {
        assert_eq!(
            strip_heartbeat_token("`HEARTBEAT_OK` Your server is down"),
            HeartbeatResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn backtick_token_at_end_with_content() {
        assert_eq!(
            strip_heartbeat_token("Your server is down `HEARTBEAT_OK`"),
            HeartbeatResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn empty_string_suppresses() {
        assert_eq!(strip_heartbeat_token(""), HeartbeatResult::Suppress);
    }

    #[test]
    fn whitespace_only_suppresses() {
        assert_eq!(strip_heartbeat_token("   "), HeartbeatResult::Suppress);
    }

    #[test]
    fn token_at_start_with_real_content() {
        assert_eq!(
            strip_heartbeat_token("HEARTBEAT_OK. Also, you have a meeting at 3pm"),
            HeartbeatResult::Deliver("Also, you have a meeting at 3pm".to_owned()),
        );
    }

    #[test]
    fn token_at_end_with_real_content() {
        assert_eq!(
            strip_heartbeat_token("Your server is down. HEARTBEAT_OK"),
            HeartbeatResult::Deliver("Your server is down.".to_owned()),
        );
    }

    #[test]
    fn no_token_real_content() {
        assert_eq!(
            strip_heartbeat_token("Your server is down"),
            HeartbeatResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn token_mid_sentence_not_stripped() {
        let text = "The status is HEARTBEAT_OK and everything is fine";
        assert_eq!(
            strip_heartbeat_token(text),
            HeartbeatResult::Deliver(text.to_owned()),
        );
    }

    #[test]
    fn token_at_both_edges_delivers_empty_suppresses() {
        assert_eq!(
            strip_heartbeat_token("HEARTBEAT_OK HEARTBEAT_OK"),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn bold_token_at_start_with_content() {
        assert_eq!(
            strip_heartbeat_token("**HEARTBEAT_OK** Your server is down"),
            HeartbeatResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn token_with_newlines() {
        assert_eq!(
            strip_heartbeat_token("\n  HEARTBEAT_OK\n  "),
            HeartbeatResult::Suppress,
        );
    }

    #[test]
    fn token_at_end_with_trailing_period() {
        assert_eq!(
            strip_heartbeat_token("All clear. HEARTBEAT_OK"),
            HeartbeatResult::Deliver("All clear.".to_owned()),
        );
    }

    #[test]
    fn real_content_with_period_at_end() {
        assert_eq!(
            strip_heartbeat_token("Your server is down."),
            HeartbeatResult::Deliver("Your server is down.".to_owned()),
        );
    }

    // -- is_heartbeat_content_empty tests --

    #[test]
    fn empty_file_is_empty() {
        assert!(is_heartbeat_content_empty(""));
    }

    #[test]
    fn whitespace_only_file_is_empty() {
        assert!(is_heartbeat_content_empty("   \n  \n  "));
    }

    #[test]
    fn headers_only_is_empty() {
        assert!(is_heartbeat_content_empty("# Heartbeat\n## Tasks\n"));
    }

    #[test]
    fn empty_checklist_is_empty() {
        assert!(is_heartbeat_content_empty("# Tasks\n- [ ]\n- [ ]\n- [x]\n"));
    }

    #[test]
    fn empty_list_items_is_empty() {
        assert!(is_heartbeat_content_empty("# Tasks\n- \n- \n"));
    }

    #[test]
    fn file_with_real_content_is_not_empty() {
        assert!(!is_heartbeat_content_empty("# Tasks\n- [ ] Deploy v2.0\n"));
    }

    #[test]
    fn mixed_empty_and_real_is_not_empty() {
        assert!(!is_heartbeat_content_empty(
            "# Heartbeat\n- [ ]\n- Check server status\n"
        ));
    }

    #[test]
    fn bare_dash_is_empty() {
        assert!(is_heartbeat_content_empty("-\n-\n"));
    }

    #[test]
    fn content_after_header_is_not_empty() {
        assert!(!is_heartbeat_content_empty("# Heartbeat\nServer is down\n"));
    }
}
