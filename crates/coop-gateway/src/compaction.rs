//! Context compaction — summarizes old conversation history when it grows
//! too large, keeping the context window bounded.
//!
//! Follows the approach from `@anthropic-ai/sdk` v0.73.0's `BetaToolRunner`:
//! when total tokens exceed a threshold, the full conversation is summarized
//! into a single user message that replaces the entire history.

use anyhow::Result;
use coop_core::traits::Provider;
use coop_core::types::{Content, Message, Role, Usage};
use tracing::{Instrument, info, info_span};

/// Compact when total tokens exceeds this.
/// Matches Anthropic SDK's `DEFAULT_TOKEN_THRESHOLD`.
pub(crate) const COMPACTION_THRESHOLD: u32 = 100_000;

const SUMMARY_PROMPT: &str = "You have been working on the task described above but have not yet completed it. Write a continuation summary that will allow you (or another instance of yourself) to resume work efficiently in a future context window where the conversation history will be replaced with this summary. Your summary should be structured, concise, and actionable. Include:\n\
1. Task Overview — The user's core request and success criteria, any clarifications or constraints\n\
2. Current State — What has been completed, files created/modified/analyzed (with paths), key outputs\n\
3. Important Discoveries — Technical constraints, decisions made and rationale, errors and resolutions, approaches that didn't work\n\
4. Next Steps — Specific actions needed, blockers or open questions, priority order\n\
5. Context to Preserve — User preferences, domain-specific details, promises made\n\
Be concise but complete—err on the side of including information that would prevent duplicate work or repeated mistakes. Write in a way that enables immediate resumption of the task.\n\
Wrap your summary in <summary></summary> tags.";

/// Persisted compaction state for a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactionState {
    /// LLM-generated structured summary.
    pub summary: String,
    /// Total tokens at time of compaction.
    pub tokens_at_compaction: u32,
    /// Timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Returns true if total usage exceeds the compaction threshold.
pub(crate) fn should_compact(usage: &Usage) -> bool {
    let total = usage.input_tokens.unwrap_or(0)
        + usage.cache_read_tokens.unwrap_or(0)
        + usage.cache_write_tokens.unwrap_or(0)
        + usage.output_tokens.unwrap_or(0);
    total > COMPACTION_THRESHOLD
}

/// Build messages for the compaction summarization call.
///
/// Strips `ToolRequest` blocks from the last assistant message to avoid
/// Anthropic 400 errors (orphaned tool_use without tool_result).
fn prepare_compaction_messages(messages: &[Message]) -> Vec<Message> {
    let mut msgs = messages.to_vec();

    // Strip tool_use from last assistant message
    if let Some(last) = msgs.last_mut()
        && last.role == Role::Assistant
    {
        last.content
            .retain(|c| !matches!(c, Content::ToolRequest { .. }));
        if last.content.is_empty() {
            msgs.pop();
        }
    }

    // Append summary prompt
    msgs.push(Message::user().with_text(SUMMARY_PROMPT));
    msgs
}

/// Run compaction: ask the provider to summarize the conversation.
pub(crate) async fn compact(
    messages: &[Message],
    provider: &dyn Provider,
    system_prompt: &str,
) -> Result<CompactionState> {
    let span = info_span!("compaction", message_count = messages.len());

    async {
        let msgs = prepare_compaction_messages(messages);

        info!(
            prepared_message_count = msgs.len(),
            "sending compaction request"
        );

        let (response, usage) = provider.complete(system_prompt, &msgs, &[]).await?;

        let summary = response
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let total_tokens = usage.input_tokens.unwrap_or(0)
            + usage.cache_read_tokens.unwrap_or(0)
            + usage.cache_write_tokens.unwrap_or(0)
            + usage.output_tokens.unwrap_or(0);

        info!(
            summary_len = summary.len(),
            compaction_tokens = total_tokens,
            "compaction complete"
        );

        Ok(CompactionState {
            summary,
            tokens_at_compaction: total_tokens,
            created_at: chrono::Utc::now(),
        })
    }
    .instrument(span)
    .await
}

/// Build the message context for a provider call, applying compaction
/// if a compaction state exists.
///
/// With compaction: returns `[summary_user_message] + messages added after compaction`.
/// Without compaction: returns all messages unchanged.
pub(crate) fn build_provider_context(
    all_messages: &[Message],
    compaction: Option<&CompactionState>,
    messages_before_compaction: usize,
) -> Vec<Message> {
    let Some(state) = compaction else {
        return all_messages.to_vec();
    };

    let summary_msg = Message::user().with_text(&state.summary);

    if messages_before_compaction >= all_messages.len() {
        // Only summary, no new messages since compaction
        vec![summary_msg]
    } else {
        // Summary + messages added after compaction
        let mut context = vec![summary_msg];
        context.extend_from_slice(&all_messages[messages_before_compaction..]);
        context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn below_threshold_does_not_compact() {
        let usage = Usage {
            input_tokens: Some(50_000),
            output_tokens: Some(10_000),
            ..Default::default()
        };
        assert!(!should_compact(&usage));
    }

    #[test]
    fn above_threshold_triggers_compaction() {
        let usage = Usage {
            input_tokens: Some(80_000),
            output_tokens: Some(30_000),
            ..Default::default()
        };
        assert!(should_compact(&usage));
    }

    #[test]
    fn above_threshold_with_cache_tokens() {
        let usage = Usage {
            input_tokens: Some(10_000),
            output_tokens: Some(5_000),
            cache_read_tokens: Some(80_000),
            cache_write_tokens: Some(10_000),
            ..Default::default()
        };
        assert!(should_compact(&usage));
    }

    #[test]
    fn build_context_without_compaction_returns_all() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_text("hi"),
        ];

        let context = build_provider_context(&messages, None, 0);
        assert_eq!(context.len(), 2);
        assert_eq!(context[0].text(), "hello");
    }

    #[test]
    fn build_context_with_compaction_returns_summary_only() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_text("hi"),
        ];

        let state = CompactionState {
            summary: "<summary>task summary</summary>".into(),
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
        };

        // All messages existed before compaction
        let context = build_provider_context(&messages, Some(&state), messages.len());
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].text(), "<summary>task summary</summary>");
    }

    #[test]
    fn build_context_with_compaction_and_new_messages() {
        let messages = vec![
            Message::user().with_text("old message 1"),
            Message::assistant().with_text("old response"),
            Message::user().with_text("new message after compaction"),
            Message::assistant().with_text("new response"),
        ];

        let state = CompactionState {
            summary: "<summary>task summary</summary>".into(),
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
        };

        // 2 messages existed before compaction, 2 added after
        let context = build_provider_context(&messages, Some(&state), 2);
        assert_eq!(context.len(), 3); // summary + 2 new messages
        assert_eq!(context[0].text(), "<summary>task summary</summary>");
        assert_eq!(context[1].text(), "new message after compaction");
        assert_eq!(context[2].text(), "new response");
    }

    #[test]
    fn strip_tool_use_from_last_assistant_message() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check.")
                .with_tool_request("t1", "bash", json!({"command": "ls"})),
        ];

        let prepared = prepare_compaction_messages(&messages);

        // Last assistant message should have text but no tool_request
        assert_eq!(prepared.len(), 3); // user, assistant (stripped), summary prompt
        let assistant = &prepared[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.content.len(), 1);
        assert!(assistant.content[0].as_text().is_some());
        assert!(!assistant.has_tool_requests());
    }

    #[test]
    fn strip_tool_use_removes_empty_assistant_message() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_tool_request("t1", "bash", json!({"command": "ls"})),
        ];

        let prepared = prepare_compaction_messages(&messages);

        // Assistant message was only tool_use, so it's removed entirely
        assert_eq!(prepared.len(), 2); // user + summary prompt
        assert_eq!(prepared[0].text(), "hello");
        assert!(prepared[1].text().contains("continuation summary"));
    }
}
