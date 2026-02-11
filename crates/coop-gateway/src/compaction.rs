//! Context compaction — summarizes old conversation history when it grows
//! too large, keeping the context window bounded.
//!
//! Follows the approach from `@anthropic-ai/sdk` v0.73.0's `BetaToolRunner`:
//! when total tokens exceed a threshold, the full conversation is summarized
//! into a single user message that replaces the entire history.

use anyhow::Result;
use coop_core::traits::Provider;
use coop_core::types::{Content, Message, Role};
use tracing::{Instrument, debug, info_span};

/// Compact when total tokens exceeds this.
/// Matches Anthropic SDK's `DEFAULT_TOKEN_THRESHOLD`.
pub(crate) const COMPACTION_THRESHOLD: u32 = 175_000;

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
    /// Number of messages in the session when compaction was performed.
    /// Used to determine which messages are "new" after compaction.
    /// If `None` (old persisted state), falls back to current session length.
    #[serde(default)]
    pub messages_at_compaction: Option<usize>,
}

/// Returns true if the given input token count exceeds the compaction threshold.
///
/// Call with the input tokens from the most recent provider response — this
/// reflects how large the context actually was for that call.
pub(crate) fn should_compact(input_tokens: u32) -> bool {
    input_tokens > COMPACTION_THRESHOLD
}

/// Build messages for the compaction summarization call.
///
/// Ensures every `tool_use` block in an assistant message has a matching
/// `tool_result` in the immediately following user message. Orphaned
/// tool_use blocks (e.g. from cancelled turns) are stripped to avoid
/// Anthropic 400 errors.
fn prepare_compaction_messages(messages: &[Message]) -> Vec<Message> {
    let mut msgs = messages.to_vec();

    // Walk assistant messages and strip any tool_use ids that lack a
    // matching tool_result in the next message.
    let mut i = 0;
    while i < msgs.len() {
        if msgs[i].role == Role::Assistant && msgs[i].has_tool_requests() {
            // Collect tool_result ids from the next message (if it exists and is a user message)
            let result_ids: std::collections::HashSet<String> =
                if i + 1 < msgs.len() && msgs[i + 1].role == Role::User {
                    msgs[i + 1]
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            Content::ToolResult { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect()
                } else {
                    std::collections::HashSet::new()
                };

            // Strip tool_use blocks whose id has no matching tool_result
            msgs[i].content.retain(|c| match c {
                Content::ToolRequest { id, .. } => result_ids.contains(id),
                _ => true,
            });

            // If the assistant message is now empty, remove it
            if msgs[i].content.is_empty() {
                msgs.remove(i);
                continue; // don't increment — next element shifted into i
            }
        }
        i += 1;
    }

    // Append summary prompt
    msgs.push(Message::user().with_text(SUMMARY_PROMPT));
    msgs
}

/// Run compaction: ask the provider to summarize the conversation.
pub(crate) async fn compact(
    messages: &[Message],
    provider: &dyn Provider,
    system_prompt: &[String],
) -> Result<CompactionState> {
    let span = info_span!("compaction", message_count = messages.len());

    async {
        let msgs = prepare_compaction_messages(messages);

        debug!(
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

        debug!(
            summary_len = summary.len(),
            compaction_tokens = total_tokens,
            "compaction complete"
        );

        Ok(CompactionState {
            summary,
            tokens_at_compaction: total_tokens,
            created_at: chrono::Utc::now(),
            messages_at_compaction: None, // set by caller
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
        assert!(!should_compact(50_000));
    }

    #[test]
    fn above_threshold_triggers_compaction() {
        assert!(should_compact(200_000));
    }

    #[test]
    fn exactly_at_threshold_does_not_compact() {
        assert!(!should_compact(COMPACTION_THRESHOLD));
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
            messages_at_compaction: Some(2),
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
            messages_at_compaction: Some(2),
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

    #[test]
    fn strip_orphaned_tool_use_mid_conversation() {
        // Simulates a cancelled turn: assistant sent tool_use, but the next
        // message is a new user message (no tool_result).
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check.")
                .with_tool_request("t1", "bash", json!({"command": "ls"})),
            // No tool_result — user sent a new message instead (cancelled turn)
            Message::user().with_text("never mind, do something else"),
            Message::assistant().with_text("Sure, doing something else."),
        ];

        let prepared = prepare_compaction_messages(&messages);

        // tool_use should be stripped from assistant[1], text kept
        assert_eq!(prepared.len(), 5); // user, assistant(text only), user, assistant, summary
        assert!(!prepared[1].has_tool_requests());
        assert_eq!(prepared[1].content.len(), 1);
        assert!(prepared[1].content[0].as_text().is_some());
    }

    #[test]
    fn keep_matched_tool_use_mid_conversation() {
        // Normal turn: tool_use followed by tool_result
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check.")
                .with_tool_request("t1", "bash", json!({"command": "ls"})),
            Message::user().with_tool_result("t1", "file1.txt\nfile2.txt", false),
            Message::assistant().with_text("Found two files."),
        ];

        let prepared = prepare_compaction_messages(&messages);

        // tool_use should be preserved — it has a matching tool_result
        assert_eq!(prepared.len(), 5); // all 4 + summary prompt
        assert!(prepared[1].has_tool_requests());
    }

    #[test]
    fn strip_orphaned_tool_use_only_removes_unmatched() {
        // Assistant has two tool_use: one matched, one orphaned
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check both.")
                .with_tool_request("t1", "bash", json!({"command": "ls"}))
                .with_tool_request("t2", "bash", json!({"command": "pwd"})),
            // Only t1 has a result (t2 was from a cancelled execution)
            Message::user().with_tool_result("t1", "file1.txt", false),
            Message::assistant().with_text("Done."),
        ];

        let prepared = prepare_compaction_messages(&messages);

        // t1 should be kept, t2 should be stripped
        let assistant = &prepared[1];
        assert_eq!(assistant.content.len(), 2); // text + t1
        assert!(assistant.content[0].as_text().is_some());
        assert!(matches!(&assistant.content[1], Content::ToolRequest { id, .. } if id == "t1"));
    }
}
