use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_core::{Message, Provider, ToolDef};
use coop_memory::{ReconcileDecision, ReconcileObservation, ReconcileRequest, Reconciler};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tracing::{info, instrument};

pub(crate) struct ProviderReconciler {
    provider: Arc<dyn Provider>,
}

impl std::fmt::Debug for ProviderReconciler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderReconciler")
            .field("provider", &self.provider.name())
            .finish_non_exhaustive()
    }
}

impl ProviderReconciler {
    pub(crate) fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Reconciler for ProviderReconciler {
    #[instrument(skip(self, request), fields(candidate_count = request.candidates.len()))]
    async fn reconcile(&self, request: &ReconcileRequest) -> Result<ReconcileDecision> {
        let system = reconciliation_system_prompt();
        let user_prompt = reconciliation_user_prompt(request)?;

        info!(
            candidate_count = request.candidates.len(),
            "memory reconciliation provider request"
        );

        let (response, _usage) = self
            .provider
            .complete_fast(
                system,
                &[Message::user().with_text(user_prompt)],
                &[] as &[ToolDef],
            )
            .await
            .context("reconciliation completion failed")?;

        let response_text = response.text();
        let decision = parse_reconciliation_response(&response_text)?;

        info!(?decision, "memory reconciliation provider decision");
        Ok(decision)
    }
}

fn reconciliation_system_prompt() -> &'static str {
    "You reconcile structured memory observations.\
Return exactly one JSON object and nothing else.\
Schema:\
{\
  \"decision\": \"ADD\" | \"UPDATE\" | \"DELETE\" | \"NONE\",\
  \"candidate_index\": integer or null,\
  \"merged\": {\
    \"store\": string,\
    \"obs_type\": string,\
    \"title\": string,\
    \"narrative\": string,\
    \"facts\": string[],\
    \"tags\": string[],\
    \"related_files\": string[],\
    \"related_people\": string[]\
  }\
}\
Rules:\
- Never reference IDs; use candidate_index only.\
- Use ADD when incoming is distinct.\
- Use UPDATE when incoming should merge into one candidate; include merged.\
- Use DELETE when one candidate is stale/incorrect and should be replaced by incoming.\
- Use NONE when incoming adds no new value and only mention_count should increase.\
- candidate_index must be null for ADD and set for UPDATE/DELETE/NONE.\
- merged is required for UPDATE and must be null otherwise."
}

fn reconciliation_user_prompt(request: &ReconcileRequest) -> Result<String> {
    let payload = serde_json::to_string_pretty(request)?;
    Ok(format!(
        "incoming and candidate set:\n{payload}\n\nRespond with strict JSON only."
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDecision {
    decision: String,
    #[serde(default)]
    candidate_index: Option<usize>,
    #[serde(default)]
    merged: Option<ReconcileObservation>,
}

fn parse_reconciliation_response(text: &str) -> Result<ReconcileDecision> {
    let value = parse_json_value(text)?;
    let raw: RawDecision = serde_json::from_value(value).context("invalid reconciliation JSON")?;

    match raw.decision.as_str() {
        "ADD" => Ok(ReconcileDecision::Add),
        "UPDATE" => {
            let candidate_index = raw
                .candidate_index
                .context("UPDATE decision missing candidate_index")?;
            let merged = raw.merged.context("UPDATE decision missing merged")?;
            Ok(ReconcileDecision::Update {
                candidate_index,
                merged,
            })
        }
        "DELETE" => Ok(ReconcileDecision::Delete {
            candidate_index: raw
                .candidate_index
                .context("DELETE decision missing candidate_index")?,
        }),
        "NONE" => Ok(ReconcileDecision::None {
            candidate_index: raw
                .candidate_index
                .context("NONE decision missing candidate_index")?,
        }),
        other => anyhow::bail!("unknown reconciliation decision '{other}'"),
    }
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
        .find('{')
        .context("reconciliation output missing JSON")?;
    let end = text
        .rfind('}')
        .context("reconciliation output missing JSON")?;
    let json = &text[start..=end];
    serde_json::from_str(json).context("failed to parse reconciliation JSON object")
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn merged() -> ReconcileObservation {
        ReconcileObservation {
            store: "shared".to_owned(),
            obs_type: "technical".to_owned(),
            title: "merged".to_owned(),
            narrative: "merged narrative".to_owned(),
            facts: vec!["fact".to_owned()],
            tags: vec!["tag".to_owned()],
            related_files: vec!["src/main.rs".to_owned()],
            related_people: vec!["alice".to_owned()],
        }
    }

    #[test]
    fn parse_add_decision() {
        let parsed = parse_reconciliation_response(
            r#"{"decision":"ADD","candidate_index":null,"merged":null}"#,
        )
        .unwrap();
        assert_eq!(parsed, ReconcileDecision::Add);
    }

    #[test]
    fn parse_update_decision() {
        let payload = serde_json::json!({
            "decision": "UPDATE",
            "candidate_index": 1,
            "merged": merged(),
        });
        let parsed = parse_reconciliation_response(&payload.to_string()).unwrap();

        assert!(matches!(
            parsed,
            ReconcileDecision::Update {
                candidate_index: 1,
                ..
            }
        ));
    }

    #[test]
    fn parse_json_from_markdown_fence() {
        let payload = serde_json::json!({
            "decision": "NONE",
            "candidate_index": 0,
            "merged": null,
        });
        let text = format!("```json\n{payload}\n```");

        let parsed = parse_reconciliation_response(&text).unwrap();
        assert_eq!(parsed, ReconcileDecision::None { candidate_index: 0 });
    }
}
