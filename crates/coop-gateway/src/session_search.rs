//! Session search tool — FTS5 search across past session transcripts
//! with LLM-powered summarization of matching sessions.
//!
//! Inspired by Hermes Agent's `session_search` tool: search past
//! conversations, then summarize the top matches with a cheap/fast model
//! so the agent gets focused recall without context bloat.

use anyhow::Result;
use async_trait::async_trait;
use coop_core::tool_args::reject_unknown_fields;
use coop_core::traits::{Provider, ToolContext, ToolExecutor};
use coop_core::types::{Content, Message, Role, ToolDef, ToolOutput, TrustLevel};
use coop_memory::{Memory, SessionMessage};
use std::sync::Arc;
use tracing::{debug, instrument, warn};

const MAX_TRANSCRIPT_CHARS: usize = 80_000;
const SUMMARIZATION_SYSTEM: &str = "\
You are reviewing a past conversation transcript to help recall what happened. \
Summarize the conversation with a focus on the search topic. Include:
1. What the user asked about or wanted to accomplish
2. What actions were taken and what the outcomes were
3. Key decisions, solutions found, or conclusions reached
4. Any specific commands, files, URLs, or technical details that were important
5. Anything left unresolved or notable

Be thorough but concise. Preserve specific details (commands, paths, error messages) \
that would be useful to recall. Write in past tense as a factual recap.";

pub(crate) struct SessionSearchExecutor {
    memory: Arc<dyn Memory>,
    provider: Arc<dyn Provider>,
}

impl SessionSearchExecutor {
    pub(crate) fn new(memory: Arc<dyn Memory>, provider: Arc<dyn Provider>) -> Self {
        Self { memory, provider }
    }
}

impl std::fmt::Debug for SessionSearchExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSearchExecutor")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ToolExecutor for SessionSearchExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        if name != "session_search" {
            anyhow::bail!("unknown tool: {name}");
        }
        exec_session_search(&self.memory, &self.provider, arguments, ctx).await
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![ToolDef::new(
            "session_search",
            "Search past conversation transcripts and get focused summaries of matching sessions. \
             Use this proactively when:\n\
             - The user says 'we did this before', 'remember when', 'last time'\n\
             - The user asks about a topic you worked on before but don't have in current context\n\
             - You want to check if you've solved a similar problem before\n\
             - The user asks 'what did we do about X?' or 'how did we fix Y?'\n\n\
             Returns LLM-generated summaries of the top matching sessions, not raw transcripts. \
             Better to search and confirm than to guess or ask the user to repeat themselves.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query — keywords or phrases to find in past sessions."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max sessions to summarize (default: 3, max: 5).",
                        "minimum": 1,
                        "maximum": 5
                    }
                },
                "required": ["query"]
            }),
        )]
    }
}

#[instrument(skip(memory, provider, arguments, ctx))]
async fn exec_session_search(
    memory: &Arc<dyn Memory>,
    provider: &Arc<dyn Provider>,
    arguments: serde_json::Value,
    ctx: &ToolContext,
) -> Result<ToolOutput> {
    if ctx.trust > TrustLevel::Inner {
        return Ok(ToolOutput::error(
            "session_search requires Inner trust or higher",
        ));
    }

    if let Some(output) = reject_unknown_fields("session_search", &arguments, &["query", "limit"]) {
        return Ok(output);
    }

    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required parameter: query"))?;

    let limit = arguments
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(3)
        .min(5) as usize;

    // Exclude very recent messages (current turn) to avoid returning
    // what the user just typed. Messages from older conversations —
    // including prior /new resets on the same channel — are found
    // because each conversation epoch gets its own search-index key.
    let exclude_since = Some(chrono::Utc::now() - chrono::Duration::seconds(10));

    // FTS5 search across session transcripts
    let hits = memory
        .search_session_messages(query, limit, exclude_since)
        .await?;

    if hits.is_empty() {
        return Ok(ToolOutput::success(
            serde_json::json!({
                "query": query,
                "results": [],
                "count": 0,
                "message": "No matching sessions found."
            })
            .to_string(),
        ));
    }

    debug!(
        query,
        hit_count = hits.len(),
        "session search found matches, summarizing"
    );

    // For each matching session, load transcript and summarize
    let mut results = Vec::new();
    for hit in &hits {
        let messages = memory.load_session_messages(&hit.session_key, 500).await?;

        if messages.is_empty() {
            continue;
        }

        let transcript = format_transcript(&messages);
        let truncated = truncate_around_query(&transcript, query, MAX_TRANSCRIPT_CHARS);

        match summarize_session(provider, query, &hit.session_key, &truncated, &hit.earliest).await
        {
            Ok(summary) => {
                results.push(serde_json::json!({
                    "session_key": hit.session_key,
                    "when": hit.earliest.format("%Y-%m-%d %H:%M").to_string(),
                    "message_count": hit.message_count,
                    "summary": summary,
                }));
            }
            Err(error) => {
                warn!(
                    session = %hit.session_key,
                    error = %error,
                    "failed to summarize session, returning snippet"
                );
                results.push(serde_json::json!({
                    "session_key": hit.session_key,
                    "when": hit.earliest.format("%Y-%m-%d %H:%M").to_string(),
                    "message_count": hit.message_count,
                    "snippet": hit.snippet,
                }));
            }
        }
    }

    let output = serde_json::json!({
        "query": query,
        "results": results,
        "count": results.len(),
    });

    Ok(ToolOutput::success(output.to_string()))
}

fn format_transcript(messages: &[SessionMessage]) -> String {
    let mut parts = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = msg.role.to_uppercase();
        let content = &msg.content;

        if let Some(tool) = &msg.tool_name {
            // Truncate long tool outputs
            let truncated = if content.len() > 500 {
                format!(
                    "{}...[truncated]...{}",
                    &content[..250],
                    &content[content.len() - 250..]
                )
            } else {
                content.clone()
            };
            parts.push(format!("[TOOL:{tool}]: {truncated}"));
        } else {
            parts.push(format!("[{role}]: {content}"));
        }
    }
    parts.join("\n\n")
}

/// Truncate transcript centered around query match locations.
fn truncate_around_query(text: &str, query: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_owned();
    }

    // Find first occurrence of any query term
    let text_lower = text.to_lowercase();
    let first_match = query
        .split_whitespace()
        .filter_map(|term| text_lower.find(&term.to_lowercase()))
        .min()
        .unwrap_or(0);

    let half = max_chars / 2;
    let start = first_match.saturating_sub(half);
    let end = (start + max_chars).min(text.len());
    let start = if end - start < max_chars {
        end.saturating_sub(max_chars)
    } else {
        start
    };

    let mut result = String::new();
    if start > 0 {
        result.push_str("...[earlier conversation truncated]...\n\n");
    }
    result.push_str(&text[start..end]);
    if end < text.len() {
        result.push_str("\n\n...[later conversation truncated]...");
    }
    result
}

async fn summarize_session(
    provider: &Arc<dyn Provider>,
    query: &str,
    session_key: &str,
    transcript: &str,
    when: &chrono::DateTime<chrono::Utc>,
) -> Result<String> {
    let user_prompt = format!(
        "Search topic: {query}\n\
         Session: {session_key}\n\
         Session date: {when}\n\n\
         CONVERSATION TRANSCRIPT:\n{transcript}\n\n\
         Summarize this conversation with focus on: {query}",
        when = when.format("%Y-%m-%d %H:%M UTC"),
    );

    let system = vec![SUMMARIZATION_SYSTEM.to_owned()];
    let messages = vec![Message::user().with_text(user_prompt)];

    let (response, _usage) = provider
        .complete_fast(&system, &messages, &[] as &[ToolDef])
        .await?;

    Ok(response.text())
}

/// Convert a Coop `Message` into `SessionMessage` entries for indexing.
pub(crate) fn messages_to_session_messages(
    session_key: &str,
    messages: &[Message],
) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        for content in &msg.content {
            match content {
                Content::Text { text } if !text.trim().is_empty() => {
                    out.push(SessionMessage {
                        session_key: session_key.to_owned(),
                        role: role.to_owned(),
                        content: text.clone(),
                        tool_name: None,
                        created_at: msg.created,
                    });
                }
                Content::ToolRequest {
                    name, arguments, ..
                } => {
                    let args_preview = arguments.to_string();
                    let args_short = if args_preview.len() > 200 {
                        format!("{}...", &args_preview[..200])
                    } else {
                        args_preview
                    };
                    out.push(SessionMessage {
                        session_key: session_key.to_owned(),
                        role: "assistant".to_owned(),
                        content: format!("[tool_call: {name}({args_short})]"),
                        tool_name: Some(name.clone()),
                        created_at: msg.created,
                    });
                }
                Content::ToolResult {
                    output, is_error, ..
                } => {
                    let label = if *is_error {
                        "tool_error"
                    } else {
                        "tool_result"
                    };
                    // Truncate large tool results for indexing
                    let indexed = if output.len() > 1000 {
                        format!("{}...[truncated]", &output[..1000])
                    } else {
                        output.clone()
                    };
                    out.push(SessionMessage {
                        session_key: session_key.to_owned(),
                        role: "user".to_owned(),
                        content: format!("[{label}]: {indexed}"),
                        tool_name: None,
                        created_at: msg.created,
                    });
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        let text = "hello world";
        assert_eq!(truncate_around_query(text, "hello", 100), text);
    }

    #[test]
    fn truncate_centers_on_query() {
        let text = "a".repeat(200);
        let result = truncate_around_query(&text, "a", 50);
        assert!(result.len() <= 100); // 50 + truncation markers
    }

    #[test]
    fn format_transcript_handles_tool_messages() {
        let msgs = vec![
            SessionMessage {
                session_key: "s1".into(),
                role: "user".into(),
                content: "hello".into(),
                tool_name: None,
                created_at: chrono::Utc::now(),
            },
            SessionMessage {
                session_key: "s1".into(),
                role: "assistant".into(),
                content: "result".into(),
                tool_name: Some("bash".into()),
                created_at: chrono::Utc::now(),
            },
        ];
        let transcript = format_transcript(&msgs);
        assert!(transcript.contains("[USER]: hello"));
        assert!(transcript.contains("[TOOL:bash]: result"));
    }

    #[test]
    fn messages_to_session_messages_extracts_text() {
        let msgs = vec![
            Message::user().with_text("hi there"),
            Message::assistant().with_text("hello"),
        ];
        let indexed = messages_to_session_messages("test:main", &msgs);
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[0].role, "user");
        assert_eq!(indexed[0].content, "hi there");
        assert_eq!(indexed[1].role, "assistant");
    }

    #[test]
    fn messages_to_session_messages_skips_empty() {
        let msgs = vec![Message::user().with_text("")];
        let indexed = messages_to_session_messages("test:main", &msgs);
        assert!(indexed.is_empty());
    }
}
