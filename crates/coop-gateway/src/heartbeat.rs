/// Token-based suppression for cron delivery.
///
/// When an `as_needed` cron responds with only `NO_ACTION_NEEDED` (or the
/// legacy `HEARTBEAT_OK`) — possibly wrapped in markdown or whitespace — the
/// response is suppressed and not delivered.
pub(crate) const NO_ACTION_NEEDED_TOKEN: &str = "NO_ACTION_NEEDED";
pub(crate) const LEGACY_HEARTBEAT_OK_TOKEN: &str = "HEARTBEAT_OK";
const SUPPRESSION_TOKENS: [&str; 2] = [NO_ACTION_NEEDED_TOKEN, LEGACY_HEARTBEAT_OK_TOKEN];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SuppressionTokenResult {
    /// The response was only a suppression token (possibly wrapped in
    /// markdown/whitespace). Suppress delivery.
    Suppress,
    /// There is real content to deliver.
    Deliver(String),
}

/// Strip the suppression token from the edges of a response and decide
/// whether to suppress or deliver.
///
/// Rules:
/// - Empty / whitespace-only → Suppress
/// - Exact match or wrapped in markdown bold/italic → Suppress
/// - Token at start/end with real content → Deliver (token removed)
/// - Token mid-sentence → NOT stripped (only edges)
/// - No token, real content → Deliver as-is
pub(crate) fn strip_suppression_token(text: &str) -> SuppressionTokenResult {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return SuppressionTokenResult::Suppress;
    }

    let unwrapped = unwrap_markdown(trimmed);
    if SUPPRESSION_TOKENS.contains(&unwrapped) {
        return SuppressionTokenResult::Suppress;
    }

    let mut remainder = trimmed.to_owned();
    for token in SUPPRESSION_TOKENS {
        remainder = strip_token_from_start(&remainder, token);
    }
    for token in SUPPRESSION_TOKENS {
        remainder = strip_token_from_end(&remainder, token);
    }

    let cleaned = remainder.trim();
    if cleaned.is_empty() {
        SuppressionTokenResult::Suppress
    } else {
        SuppressionTokenResult::Deliver(cleaned.to_owned())
    }
}

pub(crate) fn contains_legacy_heartbeat_token(text: &str) -> bool {
    text.contains(LEGACY_HEARTBEAT_OK_TOKEN)
}

/// Strip leading markdown (**, *, `) wrappers to expose the inner text.
fn unwrap_markdown(s: &str) -> &str {
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

fn strip_token_from_start(text: &str, token: &str) -> String {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix(token) {
        strip_leading_punctuation(rest).to_owned()
    } else if let Some(rest) = trimmed
        .strip_prefix("**")
        .and_then(|r| r.strip_prefix(token))
        .and_then(|r| r.strip_prefix("**"))
    {
        strip_leading_punctuation(rest).to_owned()
    } else if let Some(rest) = trimmed
        .strip_prefix('*')
        .and_then(|r| r.strip_prefix(token))
        .and_then(|r| r.strip_prefix('*'))
    {
        strip_leading_punctuation(rest).to_owned()
    } else if let Some(rest) = trimmed
        .strip_prefix('`')
        .and_then(|r| r.strip_prefix(token))
        .and_then(|r| r.strip_prefix('`'))
    {
        strip_leading_punctuation(rest).to_owned()
    } else {
        text.to_owned()
    }
}

fn strip_token_from_end(text: &str, token: &str) -> String {
    let trimmed = text.trim_end();
    if let Some(rest) = trimmed.strip_suffix(token) {
        rest.trim_end().to_owned()
    } else if let Some(rest) = trimmed
        .strip_suffix("**")
        .and_then(|r| r.strip_suffix(token))
        .and_then(|r| r.strip_suffix("**"))
    {
        rest.trim_end().to_owned()
    } else if let Some(rest) = trimmed
        .strip_suffix('*')
        .and_then(|r| r.strip_suffix(token))
        .and_then(|r| r.strip_suffix('*'))
    {
        rest.trim_end().to_owned()
    } else if let Some(rest) = trimmed
        .strip_suffix('`')
        .and_then(|r| r.strip_suffix(token))
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
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "-" || trimmed == "- [ ]" || trimmed == "- [x]" || trimmed == "- [X]" {
            continue;
        }
        return false;
    }
    true
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_suppresses() {
        assert_eq!(
            strip_suppression_token("NO_ACTION_NEEDED"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn legacy_exact_match_suppresses() {
        assert_eq!(
            strip_suppression_token("HEARTBEAT_OK"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn with_whitespace_suppresses() {
        assert_eq!(
            strip_suppression_token("  NO_ACTION_NEEDED  "),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn markdown_bold_suppresses() {
        assert_eq!(
            strip_suppression_token("**NO_ACTION_NEEDED**"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn markdown_italic_suppresses() {
        assert_eq!(
            strip_suppression_token("*NO_ACTION_NEEDED*"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn markdown_backtick_suppresses() {
        assert_eq!(
            strip_suppression_token("`NO_ACTION_NEEDED`"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn backtick_token_at_start_with_content() {
        assert_eq!(
            strip_suppression_token("`NO_ACTION_NEEDED` Your server is down"),
            SuppressionTokenResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn backtick_token_at_end_with_content() {
        assert_eq!(
            strip_suppression_token("Your server is down `NO_ACTION_NEEDED`"),
            SuppressionTokenResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn empty_string_suppresses() {
        assert_eq!(
            strip_suppression_token(""),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn whitespace_only_suppresses() {
        assert_eq!(
            strip_suppression_token("   "),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn token_at_start_with_real_content() {
        assert_eq!(
            strip_suppression_token("NO_ACTION_NEEDED. Also, you have a meeting at 3pm"),
            SuppressionTokenResult::Deliver("Also, you have a meeting at 3pm".to_owned()),
        );
    }

    #[test]
    fn token_at_end_with_real_content() {
        assert_eq!(
            strip_suppression_token("Your server is down. NO_ACTION_NEEDED"),
            SuppressionTokenResult::Deliver("Your server is down.".to_owned()),
        );
    }

    #[test]
    fn no_token_real_content() {
        assert_eq!(
            strip_suppression_token("Your server is down"),
            SuppressionTokenResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn token_mid_sentence_not_stripped() {
        let text = "The status is NO_ACTION_NEEDED and everything is fine";
        assert_eq!(
            strip_suppression_token(text),
            SuppressionTokenResult::Deliver(text.to_owned()),
        );
    }

    #[test]
    fn token_at_both_edges_delivers_empty_suppresses() {
        assert_eq!(
            strip_suppression_token("NO_ACTION_NEEDED NO_ACTION_NEEDED"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn legacy_and_new_tokens_at_edges_suppress() {
        assert_eq!(
            strip_suppression_token("HEARTBEAT_OK NO_ACTION_NEEDED"),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn bold_token_at_start_with_content() {
        assert_eq!(
            strip_suppression_token("**NO_ACTION_NEEDED** Your server is down"),
            SuppressionTokenResult::Deliver("Your server is down".to_owned()),
        );
    }

    #[test]
    fn token_with_newlines() {
        assert_eq!(
            strip_suppression_token("\n  NO_ACTION_NEEDED\n  "),
            SuppressionTokenResult::Suppress,
        );
    }

    #[test]
    fn token_at_end_with_trailing_period() {
        assert_eq!(
            strip_suppression_token("All clear. NO_ACTION_NEEDED"),
            SuppressionTokenResult::Deliver("All clear.".to_owned()),
        );
    }

    #[test]
    fn real_content_with_period_at_end() {
        assert_eq!(
            strip_suppression_token("Your server is down."),
            SuppressionTokenResult::Deliver("Your server is down.".to_owned()),
        );
    }

    #[test]
    fn detects_legacy_token() {
        assert!(contains_legacy_heartbeat_token("HEARTBEAT_OK"));
        assert!(!contains_legacy_heartbeat_token("NO_ACTION_NEEDED"));
    }

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
