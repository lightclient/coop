//! Context compaction — summarizes old conversation history when it grows
//! too large, keeping the context window bounded.
//!
//! Supports iterative compaction: when a previous summary exists, only new
//! messages are summarized and merged into the existing summary. A cut-point
//! preserves recent context verbatim. File operations are tracked across
//! compactions.

use anyhow::Result;
use coop_core::traits::Provider;
use coop_core::types::{Content, Message, Role};
use tracing::{Instrument, debug, info_span};

/// Compact when total tokens exceeds this.
pub(crate) const COMPACTION_THRESHOLD: u32 = 175_000;

/// Approximate tokens of recent context to preserve verbatim after compaction.
const RECENT_CONTEXT_TARGET: u32 = 20_000;

/// Rough chars-per-token estimate for cut-point detection.
const CHARS_PER_TOKEN: u32 = 4;

const FIRST_SUMMARY_PROMPT: &str = "You have been working on the task described above but have not yet completed it. Write a continuation summary that will allow you (or another instance of yourself) to resume work efficiently in a future context window where the conversation history will be replaced with this summary. Your summary should be structured, concise, and actionable. Include:\n\
1. Task Overview — The user's core request and success criteria, any clarifications or constraints\n\
2. Current State — What has been completed, files created/modified/analyzed (with paths), key outputs\n\
3. Important Discoveries — Technical constraints, decisions made and rationale, errors and resolutions, approaches that didn't work\n\
4. Next Steps — Specific actions needed, blockers or open questions, priority order\n\
5. Context to Preserve — User preferences, domain-specific details, promises made\n\
6. Files Touched — List all files that were read, created, modified, or deleted, with their paths and the action taken\n\
Be concise but complete—err on the side of including information that would prevent duplicate work or repeated mistakes. Write in a way that enables immediate resumption of the task.\n\
Wrap your summary in <summary></summary> tags.";

const UPDATE_SUMMARY_PROMPT_PREFIX: &str =
    "Here is your previous continuation summary:\n<previous_summary>\n";
const UPDATE_SUMMARY_PROMPT_SUFFIX: &str = "\n</previous_summary>\n\n\
The following new conversation has occurred since that summary was written.\n\
Update the summary to incorporate the new information. Preserve important\n\
details from the previous summary. Do NOT regenerate from scratch — merge\n\
the new information into the existing summary structure.\n\
Your updated summary should include:\n\
1. Task Overview — Updated with any new goals or constraints\n\
2. Current State — Updated with new completions, files created/modified/analyzed\n\
3. Important Discoveries — Merge new findings with previous ones\n\
4. Next Steps — Updated priority list\n\
5. Context to Preserve — Updated preferences, promises\n\
6. Files Touched — Complete list of all files read, created, modified, or deleted across both the previous summary and the new work\n\
Wrap your summary in <summary></summary> tags.";

/// What happened to a file during the conversation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) enum FileAction {
    Read,
    Created,
    Modified,
    Deleted,
}

impl std::fmt::Display for FileAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read => write!(f, "READ"),
            Self::Created => write!(f, "CREATED"),
            Self::Modified => write!(f, "MODIFIED"),
            Self::Deleted => write!(f, "DELETED"),
        }
    }
}

/// A file that was touched during the conversation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct FileTouched {
    pub path: String,
    pub action: FileAction,
}

/// Persisted compaction state for a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactionState {
    pub summary: String,
    #[serde(default)]
    pub files_touched: Vec<FileTouched>,
    #[serde(default)]
    pub compaction_count: u32,
    pub tokens_at_compaction: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub messages_at_compaction: Option<usize>,
}

/// Returns true if the given input token count exceeds the compaction threshold.
pub(crate) fn should_compact(input_tokens: u32) -> bool {
    input_tokens > COMPACTION_THRESHOLD
}

/// Find the cut point: index into `messages` such that everything from
/// `cut_point..` represents approximately `RECENT_CONTEXT_TARGET` tokens.
///
/// Everything before the cut point gets summarized; everything after is
/// kept verbatim.
///
/// The returned index always points to an assistant message (or past the
/// end). Because the compaction summary is injected as a user message,
/// the first kept message must be an assistant for proper role
/// alternation. Landing on a user message with `tool_result` blocks
/// would also orphan those results (the matching `tool_use` is before
/// the cut and gets summarized away), causing the API to reject the
/// request. By including such boundary user messages in the summarized
/// portion, the summary captures the complete tool interaction.
fn find_cut_point(messages: &[Message]) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let target_chars = (RECENT_CONTEXT_TARGET * CHARS_PER_TOKEN) as usize;
    let mut char_count = 0;

    for (i, msg) in messages.iter().enumerate().rev() {
        let msg_chars = estimate_message_chars(msg);
        char_count += msg_chars;
        if char_count >= target_chars {
            // Advance past any user messages so the kept portion starts
            // with an assistant message. This keeps tool_use/tool_result
            // pairs intact in the summarized portion rather than splitting
            // them across the boundary.
            let mut cut = i;
            while cut < messages.len() && messages[cut].role == Role::User {
                cut += 1;
            }
            return cut;
        }
    }

    // All messages fit within the target — summarize nothing
    0
}

fn estimate_message_chars(msg: &Message) -> usize {
    msg.content
        .iter()
        .map(|c| match c {
            Content::Text { text } => text.len(),
            Content::ToolRequest { arguments, .. } => {
                arguments.to_string().len().min(500) // cap tool args contribution
            }
            Content::ToolResult { output, .. } => output.len().min(2000), // cap tool output
            Content::Image { .. } => 1000, // rough estimate for images
            Content::Thinking { thinking, .. } => thinking.len(),
        })
        .sum()
}

/// Extract file operations from messages by scanning tool_request content blocks.
fn extract_files_touched(messages: &[Message]) -> Vec<FileTouched> {
    let mut files: Vec<FileTouched> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for msg in messages {
        for content in &msg.content {
            if let Content::ToolRequest {
                name, arguments, ..
            } = content
            {
                let entries = match name.as_str() {
                    "read_file" => {
                        if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                            vec![FileTouched {
                                path: path.to_owned(),
                                action: FileAction::Read,
                            }]
                        } else {
                            vec![]
                        }
                    }
                    "write_file" => {
                        if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                            vec![FileTouched {
                                path: path.to_owned(),
                                action: FileAction::Created,
                            }]
                        } else {
                            vec![]
                        }
                    }
                    "edit_file" => {
                        if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                            vec![FileTouched {
                                path: path.to_owned(),
                                action: FileAction::Modified,
                            }]
                        } else {
                            vec![]
                        }
                    }
                    "bash" => extract_bash_file_ops(arguments),
                    _ => vec![],
                };

                for entry in entries {
                    let key = (entry.path.clone(), entry.action.to_string());
                    if seen.insert(key) {
                        files.push(entry);
                    }
                }
            }
        }
    }

    files
}

/// Best-effort extraction of file operations from bash commands.
fn extract_bash_file_ops(arguments: &serde_json::Value) -> Vec<FileTouched> {
    let Some(cmd) = arguments.get("command").and_then(|v| v.as_str()) else {
        return vec![];
    };

    let mut results = Vec::new();

    // Simple pattern matching for common file operations
    for part in cmd.split("&&").chain(cmd.split(';')) {
        let trimmed = part.trim();
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        match tokens[0] {
            "rm" | "rm -rf" | "rm -f" => {
                for &token in &tokens[1..] {
                    if !token.starts_with('-') {
                        results.push(FileTouched {
                            path: token.to_owned(),
                            action: FileAction::Deleted,
                        });
                    }
                }
            }
            "mv" if tokens.len() >= 3 => {
                results.push(FileTouched {
                    path: tokens[tokens.len() - 1].to_owned(),
                    action: FileAction::Modified,
                });
            }
            "cp" if tokens.len() >= 3 => {
                results.push(FileTouched {
                    path: tokens[tokens.len() - 1].to_owned(),
                    action: FileAction::Created,
                });
            }
            _ => {}
        }
    }

    results
}

/// Merge new file touches into an existing list, deduplicating.
/// Later actions (e.g. Modified after Read) take precedence.
fn merge_files_touched(existing: &[FileTouched], new: &[FileTouched]) -> Vec<FileTouched> {
    let mut map: std::collections::HashMap<String, FileAction> = std::collections::HashMap::new();

    // Insert existing first
    for f in existing {
        map.insert(f.path.clone(), f.action.clone());
    }

    // New entries overwrite (later action takes precedence)
    for f in new {
        let entry = map
            .entry(f.path.clone())
            .or_insert_with(|| f.action.clone());
        // Upgrade: Read → Modified/Created/Deleted; but don't downgrade
        match (&entry, &f.action) {
            (FileAction::Read, action) => *entry = action.clone(),
            (_, FileAction::Deleted) => *entry = FileAction::Deleted,
            _ => {} // keep existing non-Read action
        }
    }

    map.into_iter()
        .map(|(path, action)| FileTouched { path, action })
        .collect()
}

/// Build the summary prompt for compaction, including file tracking info.
fn build_summary_prompt(previous_summary: Option<&str>, files_touched: &[FileTouched]) -> String {
    let mut prompt = String::new();

    if let Some(prev) = previous_summary {
        prompt.push_str(UPDATE_SUMMARY_PROMPT_PREFIX);
        prompt.push_str(prev);
        prompt.push_str(UPDATE_SUMMARY_PROMPT_SUFFIX);
    } else {
        prompt.push_str(FIRST_SUMMARY_PROMPT);
    }

    if !files_touched.is_empty() {
        prompt.push_str("\n\nAdditionally, here are the files that were read or modified during this work:\n<files_touched>\n");
        for f in files_touched {
            use std::fmt::Write;
            let _ = writeln!(prompt, "- {}: {}", f.action, f.path);
        }
        prompt.push_str("</files_touched>\nInclude this file list in your summary under a \"Files Touched\" section.");
    }

    prompt
}

/// Build messages for the compaction summarization call.
///
/// Ensures every `tool_use` block in an assistant message has a matching
/// `tool_result` in the immediately following user message. Orphaned
/// tool_use blocks (e.g. from cancelled turns) are stripped to avoid
/// Anthropic 400 errors.
fn prepare_compaction_messages(
    messages: &[Message],
    previous_summary: Option<&str>,
    files_touched: &[FileTouched],
) -> Vec<Message> {
    let mut msgs = messages.to_vec();

    // Walk assistant messages and strip any tool_use ids that lack a
    // matching tool_result in the next message.
    let mut i = 0;
    while i < msgs.len() {
        if msgs[i].role == Role::Assistant && msgs[i].has_tool_requests() {
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

            msgs[i].content.retain(|c| match c {
                Content::ToolRequest { id, .. } => result_ids.contains(id),
                _ => true,
            });

            if msgs[i].content.is_empty() {
                msgs.remove(i);
                continue;
            }
        }
        i += 1;
    }

    let summary_prompt = build_summary_prompt(previous_summary, files_touched);
    msgs.push(Message::user().with_text(summary_prompt));
    msgs
}

/// Run compaction: ask the provider to summarize the conversation.
///
/// When `previous_state` is provided, performs iterative compaction:
/// only messages after the previous cut point are sent for summarization,
/// and the previous summary is included for the model to update.
pub(crate) async fn compact(
    messages: &[Message],
    provider: &dyn Provider,
    system_prompt: &[String],
    previous_state: Option<&CompactionState>,
) -> Result<CompactionState> {
    let span = info_span!("compaction", message_count = messages.len());

    async {
        // Determine what to summarize based on cut-point detection.
        let cut_point = find_cut_point(messages);

        // For iterative compaction, only summarize messages between the
        // old cut point and the new cut point.
        let (msgs_to_summarize, previous_summary, existing_files) =
            if let Some(prev) = previous_state {
                let old_cut = prev.messages_at_compaction.unwrap_or(0);
                // Summarize from old_cut..cut_point (the gap since last compaction)
                let start = old_cut.min(cut_point);
                let slice = if start < cut_point {
                    &messages[start..cut_point]
                } else {
                    // No new messages to summarize beyond what's already in the summary.
                    // This can happen if cut_point moved backwards. Summarize a small
                    // window to refresh the summary with recent context.
                    &messages[cut_point.saturating_sub(5)..cut_point]
                };
                (
                    slice,
                    Some(prev.summary.as_str()),
                    prev.files_touched.clone(),
                )
            } else {
                (&messages[..cut_point], None, Vec::new())
            };

        // If there's nothing to summarize, return previous state or empty
        if let (true, Some(prev)) = (msgs_to_summarize.is_empty(), previous_state) {
            return Ok(CompactionState {
                summary: prev.summary.clone(),
                files_touched: prev.files_touched.clone(),
                compaction_count: prev.compaction_count,
                tokens_at_compaction: prev.tokens_at_compaction,
                created_at: chrono::Utc::now(),
                messages_at_compaction: Some(cut_point),
            });
        }

        let new_files = extract_files_touched(msgs_to_summarize);
        let all_files = merge_files_touched(&existing_files, &new_files);

        let msgs = prepare_compaction_messages(msgs_to_summarize, previous_summary, &all_files);

        debug!(
            prepared_message_count = msgs.len(),
            cut_point,
            is_iterative = previous_state.is_some(),
            compaction_count = previous_state.map_or(0, |p| p.compaction_count),
            files_tracked = all_files.len(),
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

        let compaction_count = previous_state.map_or(1, |p| p.compaction_count + 1);

        debug!(
            summary_len = summary.len(),
            compaction_tokens = total_tokens,
            compaction_count,
            "compaction complete"
        );

        Ok(CompactionState {
            summary,
            files_touched: all_files,
            compaction_count,
            tokens_at_compaction: total_tokens,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(cut_point),
        })
    }
    .instrument(span)
    .await
}

/// Build the message context for a provider call, applying compaction
/// if a compaction state exists.
///
/// With compaction: returns `[summary_user_message] + messages after cut point`.
/// Without compaction: returns all messages unchanged.
///
/// The cut point is adjusted forward when it lands on a user message with
/// `tool_result` blocks whose matching `tool_use` assistant message was
/// compacted away. Without this adjustment the API rejects the request
/// because `tool_result` blocks reference non-existent `tool_use` ids.
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
        vec![summary_msg]
    } else {
        let start = safe_cut_start(all_messages, messages_before_compaction);
        let mut context = vec![summary_msg];
        context.extend_from_slice(&all_messages[start..]);
        context
    }
}

/// Safety net: advance past user messages at the compaction boundary.
///
/// `find_cut_point` already returns an assistant-aligned index, but
/// persisted compaction states from before that fix (or edge cases in
/// iterative compaction) may still store a user-message index. This
/// prevents orphaned `tool_result` blocks from reaching the API.
fn safe_cut_start(messages: &[Message], mut idx: usize) -> usize {
    while idx < messages.len() && messages[idx].role == Role::User {
        idx += 1;
    }
    idx
}

#[allow(clippy::unwrap_used)]
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
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(2),
        };

        let context = build_provider_context(&messages, Some(&state), messages.len());
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].text(), "<summary>task summary</summary>");
    }

    #[test]
    fn build_context_with_compaction_and_new_messages() {
        // Cut lands on an assistant message — kept as-is.
        let messages = vec![
            Message::user().with_text("old message 1"),
            Message::assistant().with_text("old response"),
            Message::assistant().with_text("new response after compaction"),
            Message::user().with_text("follow up"),
            Message::assistant().with_text("follow up response"),
        ];

        let state = CompactionState {
            summary: "<summary>task summary</summary>".into(),
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(2),
        };

        let context = build_provider_context(&messages, Some(&state), 2);
        assert_eq!(context.len(), 4);
        assert_eq!(context[0].text(), "<summary>task summary</summary>");
        assert_eq!(context[1].text(), "new response after compaction");
        assert_eq!(context[2].text(), "follow up");
        assert_eq!(context[3].text(), "follow up response");
    }

    #[test]
    fn strip_tool_use_from_last_assistant_message() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check.")
                .with_tool_request("t1", "bash", json!({"command": "ls"})),
        ];

        let prepared = prepare_compaction_messages(&messages, None, &[]);

        assert_eq!(prepared.len(), 3);
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

        let prepared = prepare_compaction_messages(&messages, None, &[]);

        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].text(), "hello");
        assert!(prepared[1].text().contains("continuation summary"));
    }

    #[test]
    fn strip_orphaned_tool_use_mid_conversation() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check.")
                .with_tool_request("t1", "bash", json!({"command": "ls"})),
            Message::user().with_text("never mind, do something else"),
            Message::assistant().with_text("Sure, doing something else."),
        ];

        let prepared = prepare_compaction_messages(&messages, None, &[]);

        assert_eq!(prepared.len(), 5);
        assert!(!prepared[1].has_tool_requests());
        assert_eq!(prepared[1].content.len(), 1);
        assert!(prepared[1].content[0].as_text().is_some());
    }

    #[test]
    fn keep_matched_tool_use_mid_conversation() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check.")
                .with_tool_request("t1", "bash", json!({"command": "ls"})),
            Message::user().with_tool_result("t1", "file1.txt\nfile2.txt", false),
            Message::assistant().with_text("Found two files."),
        ];

        let prepared = prepare_compaction_messages(&messages, None, &[]);

        assert_eq!(prepared.len(), 5);
        assert!(prepared[1].has_tool_requests());
    }

    #[test]
    fn strip_orphaned_tool_use_only_removes_unmatched() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant()
                .with_text("Let me check both.")
                .with_tool_request("t1", "bash", json!({"command": "ls"}))
                .with_tool_request("t2", "bash", json!({"command": "pwd"})),
            Message::user().with_tool_result("t1", "file1.txt", false),
            Message::assistant().with_text("Done."),
        ];

        let prepared = prepare_compaction_messages(&messages, None, &[]);

        let assistant = &prepared[1];
        assert_eq!(assistant.content.len(), 2);
        assert!(assistant.content[0].as_text().is_some());
        assert!(matches!(&assistant.content[1], Content::ToolRequest { id, .. } if id == "t1"));
    }

    // --- Cut-point detection tests ---

    #[test]
    fn cut_point_empty_messages() {
        assert_eq!(find_cut_point(&[]), 0);
    }

    #[test]
    fn cut_point_short_conversation_returns_zero() {
        let messages = vec![
            Message::user().with_text("hello"),
            Message::assistant().with_text("hi"),
        ];
        assert_eq!(find_cut_point(&messages), 0);
    }

    #[test]
    fn cut_point_long_conversation_preserves_recent() {
        // Create messages where each is ~1000 chars (~250 tokens).
        // With 20K token target, we want ~80 messages preserved.
        let mut messages = Vec::new();
        for i in 0..200 {
            let text = format!("message {i} {}", "x".repeat(900));
            if i % 2 == 0 {
                messages.push(Message::user().with_text(&text));
            } else {
                messages.push(Message::assistant().with_text(&text));
            }
        }

        let cut = find_cut_point(&messages);
        // Cut point should leave some messages after it
        assert!(cut > 0, "should have a non-zero cut point");
        assert!(cut < 200, "should preserve some recent messages");
        // Must land on an assistant message (odd index in this test)
        assert_eq!(
            messages[cut].role,
            Role::Assistant,
            "cut point must land on an assistant message"
        );
        // The preserved part (cut..200) should be roughly 20K tokens
        let preserved_chars: usize = messages[cut..].iter().map(estimate_message_chars).sum();
        #[allow(clippy::cast_possible_truncation)]
        let preserved_tokens = preserved_chars as u32 / CHARS_PER_TOKEN;
        assert!(
            preserved_tokens >= RECENT_CONTEXT_TARGET / 2,
            "should preserve at least half the target: {preserved_tokens}"
        );
    }

    #[test]
    fn cut_point_lands_on_assistant_after_tool_result() {
        // When the raw character-based cut lands on a user tool_result,
        // it must advance to the next assistant message so the pair stays
        // together in the summarized portion.
        let mut messages = Vec::new();
        let padding = "x".repeat(800);
        for i in 0..200 {
            messages.push(Message::user().with_text(format!("input {i} {padding}")));
            messages.push(Message::assistant().with_tool_request(
                format!("t{i}"),
                "bash",
                json!({"command": "ls"}),
            ));
            messages.push(Message::user().with_tool_result(
                format!("t{i}"),
                format!("ok {padding}"),
                false,
            ));
            messages.push(Message::assistant().with_text(format!("done {i} {padding}")));
        }

        let cut = find_cut_point(&messages);
        assert!(
            cut > 0,
            "conversation should be large enough to trigger a cut"
        );
        assert_eq!(
            messages[cut].role,
            Role::Assistant,
            "cut at index {cut} is a user message"
        );
    }

    // --- File tracking tests ---

    #[test]
    fn extract_files_from_tool_requests() {
        let messages = vec![
            Message::assistant().with_tool_request(
                "t1",
                "read_file",
                json!({"path": "src/main.rs"}),
            ),
            Message::user().with_tool_result("t1", "fn main() {}", false),
            Message::assistant().with_tool_request(
                "t2",
                "write_file",
                json!({"path": "src/new.rs", "content": "pub fn new() {}"}),
            ),
            Message::user().with_tool_result("t2", "ok", false),
            Message::assistant().with_tool_request(
                "t3",
                "edit_file",
                json!({"path": "src/main.rs", "old": "x", "new": "y"}),
            ),
            Message::user().with_tool_result("t3", "ok", false),
        ];

        let files = extract_files_touched(&messages);
        assert!(
            files
                .iter()
                .any(|f| f.path == "src/main.rs" && f.action == FileAction::Read)
        );
        assert!(
            files
                .iter()
                .any(|f| f.path == "src/new.rs" && f.action == FileAction::Created)
        );
        assert!(
            files
                .iter()
                .any(|f| f.path == "src/main.rs" && f.action == FileAction::Modified)
        );
    }

    #[test]
    fn extract_bash_rm_operations() {
        let messages = vec![Message::assistant().with_tool_request(
            "t1",
            "bash",
            json!({"command": "rm -f old.txt"}),
        )];

        let files = extract_files_touched(&messages);
        assert!(
            files
                .iter()
                .any(|f| f.path == "old.txt" && f.action == FileAction::Deleted)
        );
    }

    #[test]
    fn merge_files_upgrades_read_to_modified() {
        let existing = vec![FileTouched {
            path: "src/lib.rs".into(),
            action: FileAction::Read,
        }];
        let new = vec![FileTouched {
            path: "src/lib.rs".into(),
            action: FileAction::Modified,
        }];

        let merged = merge_files_touched(&existing, &new);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].action, FileAction::Modified);
    }

    #[test]
    fn iterative_summary_prompt_includes_previous() {
        let prompt = build_summary_prompt(Some("<summary>old stuff</summary>"), &[]);
        assert!(prompt.contains("<previous_summary>"));
        assert!(prompt.contains("<summary>old stuff</summary>"));
        assert!(prompt.contains("Update the summary"));
    }

    #[test]
    fn first_summary_prompt_has_no_previous() {
        let prompt = build_summary_prompt(None, &[]);
        assert!(!prompt.contains("<previous_summary>"));
        assert!(prompt.contains("continuation summary"));
    }

    #[test]
    fn summary_prompt_includes_files_touched() {
        let files = vec![
            FileTouched {
                path: "src/main.rs".into(),
                action: FileAction::Modified,
            },
            FileTouched {
                path: "README.md".into(),
                action: FileAction::Read,
            },
        ];

        let prompt = build_summary_prompt(None, &files);
        assert!(prompt.contains("<files_touched>"));
        assert!(prompt.contains("MODIFIED: src/main.rs"));
        assert!(prompt.contains("READ: README.md"));
    }

    // --- safe_cut_start tests ---

    #[test]
    fn safe_cut_start_skips_user_tool_result_at_boundary() {
        // Reproduces the bug: cut lands on a user message with tool_results
        // whose matching tool_use was compacted away.
        let messages = vec![
            Message::user().with_text("old input"),
            Message::assistant().with_tool_request("t1", "bash", json!({"command": "ls"})),
            Message::user().with_tool_result("t1", "file.txt", false), // index 2 = cut
            Message::assistant().with_text("Found a file."),           // index 3
            Message::user().with_text("new question"),                 // index 4
            Message::assistant().with_text("answer"),                  // index 5
        ];

        let state = CompactionState {
            summary: "<summary>summary</summary>".into(),
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(2),
        };

        let context = build_provider_context(&messages, Some(&state), 2);
        // Must start with summary(user) followed by assistant — never
        // an orphaned tool_result user message.
        assert_eq!(context[0].role, Role::User);
        assert_eq!(context[0].text(), "<summary>summary</summary>");
        assert_eq!(context[1].role, Role::Assistant);
        assert_eq!(context[1].text(), "Found a file.");
        assert_eq!(context.len(), 4); // summary + messages[3..6]
    }

    #[test]
    fn safe_cut_start_skips_consecutive_user_messages() {
        // Cut lands on a user text message — still must advance past it
        // because the summary is already a user message.
        let messages = vec![
            Message::user().with_text("old"),
            Message::assistant().with_text("old reply"),
            Message::user().with_text("boundary user msg"), // index 2 = cut
            Message::assistant().with_text("reply"),        // index 3
        ];

        let state = CompactionState {
            summary: "<summary>s</summary>".into(),
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(2),
        };

        let context = build_provider_context(&messages, Some(&state), 2);
        assert_eq!(context[0].role, Role::User); // summary
        assert_eq!(context[1].role, Role::Assistant); // skipped past user
        assert_eq!(context.len(), 2);
    }

    #[test]
    fn safe_cut_start_keeps_assistant_at_boundary() {
        // Cut lands on an assistant message — no adjustment needed.
        let messages = vec![
            Message::user().with_text("old"),
            Message::assistant().with_text("compacted away"),
            Message::assistant().with_text("kept"), // index 2 = cut
            Message::user().with_text("new"),       // index 3
        ];

        let state = CompactionState {
            summary: "<summary>s</summary>".into(),
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(2),
        };

        let context = build_provider_context(&messages, Some(&state), 2);
        assert_eq!(context[0].role, Role::User); // summary
        assert_eq!(context[1].role, Role::Assistant); // messages[2]
        assert_eq!(context[1].text(), "kept");
        assert_eq!(context.len(), 3);
    }

    #[test]
    fn safe_cut_start_all_remaining_are_user() {
        // Edge case: all remaining messages are user messages.
        let messages = vec![
            Message::user().with_text("old"),
            Message::assistant().with_text("old reply"),
            Message::user().with_tool_result("t1", "result", false), // index 2
            Message::user().with_text("another user msg"),           // index 3
        ];

        let state = CompactionState {
            summary: "<summary>s</summary>".into(),
            files_touched: vec![],
            compaction_count: 1,
            tokens_at_compaction: 100_000,
            created_at: chrono::Utc::now(),
            messages_at_compaction: Some(2),
        };

        // All remaining messages are user — returns summary only.
        let context = build_provider_context(&messages, Some(&state), 2);
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].text(), "<summary>s</summary>");
    }

    // --- Backwards compatibility ---

    #[test]
    fn compaction_state_deserializes_without_new_fields() {
        let json = r#"{
            "summary": "test",
            "tokens_at_compaction": 100000,
            "created_at": "2026-01-01T00:00:00Z",
            "messages_at_compaction": 10
        }"#;

        let state: CompactionState = serde_json::from_str(json).unwrap();
        assert_eq!(state.summary, "test");
        assert!(state.files_touched.is_empty());
        assert_eq!(state.compaction_count, 0);
    }
}
