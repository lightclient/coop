use anyhow::Error;

const TRUNCATED_TOOL_USE_MARKER: &str = "invalid/incomplete input_json after max_tokens";
const CONTEXT_OVERFLOW_MARKERS: &[&str] = &[
    "context length",
    "context window",
    "maximum context length",
    "too many tokens",
    "prompt is too long",
    "input is too long",
    "request exceeds context",
];

pub(crate) fn should_force_compact_after_error(error: &Error) -> bool {
    let rendered = format!("{error:#}");
    if rendered.contains(TRUNCATED_TOOL_USE_MARKER) {
        return true;
    }

    let lower = rendered.to_ascii_lowercase();
    CONTEXT_OVERFLOW_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_truncated_tool_use_max_tokens_error() {
        let error = anyhow::anyhow!(
            "Anthropic streamed tool_use for `bash` ended with invalid/incomplete input_json after max_tokens"
        );

        assert!(should_force_compact_after_error(&error));
    }

    #[test]
    fn detects_context_overflow_error() {
        let error = anyhow::anyhow!(
            "OpenAI API error: This model's maximum context length is 128000 tokens"
        );

        assert!(should_force_compact_after_error(&error));
    }

    #[test]
    fn ignores_unrelated_errors() {
        let error = anyhow::anyhow!("Anthropic API error: 500");

        assert!(!should_force_compact_after_error(&error));
    }
}
