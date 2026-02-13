use anyhow::{Context, Result};
use coop_core::{Content, Message, Provider, SessionKey, ToolDef, TrustLevel};
use coop_memory::{NewObservation, min_trust_for_store, trust_to_store};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument, warn};

const EXTRACTION_SYSTEM_PROMPT: &str = "You are a memory extraction system. Given a single conversation turn, extract observations worth remembering for future sessions.\
Return a JSON array only (no prose, no markdown).\
Each observation object must contain:\
- title: concise summary (max 80 chars)\
- narrative: 1-2 sentence context\
- facts: array of atomic fact strings\
- type: one of discovery|decision|preference|task|event\
- tags: array of tags\
- related_people: array of names\
- related_files: array of file paths\
Rules:\
- Skip routine tool chatter, greetings, and meta-conversation.\
- If nothing is worth remembering, return [].";

#[derive(Debug, Deserialize)]
struct RawObservation {
    title: String,
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default, rename = "type")]
    obs_type: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    related_people: Vec<String>,
    #[serde(default)]
    related_files: Vec<String>,
}

#[instrument(skip(provider, messages), fields(session = %session_key, trust = ?trust, message_count = messages.len()))]
pub(crate) async fn extract_turn_observations(
    provider: &dyn Provider,
    messages: &[Message],
    session_key: &SessionKey,
    trust: TrustLevel,
) -> Result<Vec<NewObservation>> {
    if messages.is_empty() {
        return Ok(Vec::new());
    }

    let user_prompt = build_extraction_prompt(messages, session_key);
    let system = vec![EXTRACTION_SYSTEM_PROMPT.to_owned()];

    let (response, _usage) = match provider
        .complete_fast(
            &system,
            &[Message::user().with_text(user_prompt)],
            &[] as &[ToolDef],
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            warn!(error = %error, "auto-capture extraction completion failed");
            return Ok(Vec::new());
        }
    };

    let raw = response.text();
    let extracted = match parse_extraction_response(&raw) {
        Ok(rows) => rows,
        Err(error) => {
            warn!(error = %error, "auto-capture extraction parse failed");
            return Ok(Vec::new());
        }
    };

    let store = trust_to_store(trust).to_owned();
    let min_trust = min_trust_for_store(&store);

    let observations = extracted
        .into_iter()
        .filter_map(|row| to_new_observation(row, session_key, &store, min_trust))
        .collect::<Vec<_>>();

    debug!(
        observation_count = observations.len(),
        "auto-capture extraction complete"
    );
    Ok(observations)
}

fn build_extraction_prompt(messages: &[Message], session_key: &SessionKey) -> String {
    let mut lines = Vec::new();
    lines.push(format!("session: {session_key}"));
    lines.push("turn_messages:".to_owned());

    for (index, message) in messages.iter().enumerate() {
        lines.push(format!(
            "{} {}",
            index + 1,
            format_message(message).replace('\n', " ")
        ));
    }

    lines.push(String::new());
    lines.push("Return strict JSON only: [...]".to_owned());
    lines.join("\n")
}

fn format_message(message: &Message) -> String {
    let role = match message.role {
        coop_core::Role::User => "user",
        coop_core::Role::Assistant => "assistant",
    };

    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            Content::Text { text } => {
                let text = text.trim();
                if !text.is_empty() {
                    parts.push(clip(text, 500));
                }
            }
            Content::ToolRequest {
                name, arguments, ..
            } => {
                let args = clip(&arguments.to_string(), 300);
                parts.push(format!("[tool_request name={name} args={args}]"));
            }
            Content::ToolResult {
                output, is_error, ..
            } => {
                parts.push(format!(
                    "[tool_result error={} output={}]",
                    is_error,
                    clip(output, 300)
                ));
            }
            Content::Image { mime_type, .. } => {
                parts.push(format!("[image mime_type={mime_type}]"));
            }
            Content::Thinking { .. } => {}
        }
    }

    format!("{role}: {}", parts.join(" "))
}

fn to_new_observation(
    raw: RawObservation,
    session_key: &SessionKey,
    store: &str,
    min_trust: TrustLevel,
) -> Option<NewObservation> {
    let title = clip(raw.title.trim(), 80);
    if title.is_empty() {
        return None;
    }

    let obs_type = normalize_type(&raw.obs_type);

    Some(NewObservation {
        session_key: Some(session_key.to_string()),
        store: store.to_owned(),
        obs_type,
        title,
        narrative: clip(raw.narrative.trim(), 400),
        facts: clean_list(raw.facts, 200),
        tags: clean_list(raw.tags, 50),
        source: "auto_capture".to_owned(),
        related_files: clean_list(raw.related_files, 200),
        related_people: clean_list(raw.related_people, 100),
        token_count: None,
        expires_at: None,
        min_trust,
    })
}

fn parse_extraction_response(text: &str) -> Result<Vec<RawObservation>> {
    let value = parse_json_value(text)?;
    let rows = serde_json::from_value::<Vec<RawObservation>>(value)
        .context("invalid auto-capture JSON payload")?;
    Ok(rows)
}

fn parse_json_value(text: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }

    if let Some(stripped) = text.trim().strip_prefix("```") {
        let stripped = stripped
            .trim_start_matches("json")
            .trim_start_matches('\n')
            .trim_end();
        let stripped = stripped.trim_end_matches("```").trim();
        if let Ok(value) = serde_json::from_str::<Value>(stripped) {
            return Ok(value);
        }
    }

    let start = text
        .find('[')
        .context("auto-capture output missing JSON array")?;
    let end = text
        .rfind(']')
        .context("auto-capture output missing JSON array")?;
    let json = &text[start..=end];
    serde_json::from_str(json).context("failed to parse auto-capture JSON array")
}

fn normalize_type(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "discovery" | "decision" | "preference" | "task" | "event" => {
            value.trim().to_ascii_lowercase()
        }
        _ => "event".to_owned(),
    }
}

fn clean_list(values: Vec<String>, max_item_chars: usize) -> Vec<String> {
    values
        .into_iter()
        .map(|value| clip(value.trim(), max_item_chars))
        .filter(|value| !value.is_empty())
        .take(20)
        .collect()
}

fn clip(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value.chars().take(max_chars).collect::<String>()
}
