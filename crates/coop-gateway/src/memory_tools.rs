use anyhow::Result;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput};
use coop_memory::{
    Memory, MemoryQuery, NewObservation, WriteOutcome, accessible_stores, min_trust_for_store,
    trust_to_store,
};
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, instrument};

#[allow(missing_debug_implementations)]
pub(crate) struct MemoryToolExecutor {
    memory: Arc<dyn Memory>,
}

impl MemoryToolExecutor {
    pub(crate) fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }

    fn defs() -> Vec<ToolDef> {
        vec![
            ToolDef::new(
                "memory_search",
                "Search structured observations. Returns compact index entries.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "stores": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Stores to search: private/shared/social"
                        },
                        "types": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "people": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "after_ms": { "type": "integer" },
                        "before_ms": { "type": "integer" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                        "max_tokens": { "type": "integer", "minimum": 1 }
                    }
                }),
            ),
            ToolDef::new(
                "memory_timeline",
                "Get observations around a specific observation ID.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "anchor": { "type": "integer" },
                        "before": { "type": "integer", "minimum": 0, "maximum": 20 },
                        "after": { "type": "integer", "minimum": 0, "maximum": 20 }
                    },
                    "required": ["anchor"]
                }),
            ),
            ToolDef::new(
                "memory_get",
                "Fetch full observation details by ID.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "ids": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "minItems": 1
                        }
                    },
                    "required": ["ids"]
                }),
            ),
            ToolDef::new(
                "memory_write",
                "Write a structured observation to memory.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "store": { "type": "string", "description": "private/shared/social" },
                        "type": { "type": "string" },
                        "title": { "type": "string" },
                        "narrative": { "type": "string" },
                        "facts": { "type": "array", "items": { "type": "string" } },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "related_files": { "type": "array", "items": { "type": "string" } },
                        "related_people": { "type": "array", "items": { "type": "string" } },
                        "source": { "type": "string" },
                        "token_count": { "type": "integer" },
                        "expires_at_ms": { "type": "integer" },
                        "session_key": { "type": "string" }
                    },
                    "required": ["title"]
                }),
            ),
            ToolDef::new(
                "memory_history",
                "Fetch mutation history for an observation.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "observation_id": { "type": "integer" }
                    },
                    "required": ["observation_id"]
                }),
            ),
            ToolDef::new(
                "memory_people",
                "Search known people promoted from observations.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    }
                }),
            ),
        ]
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_search(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let allowed = accessible_stores(ctx.trust);
        if allowed.is_empty() {
            return Ok(ToolOutput::success("{\"count\":0,\"results\":[]}"));
        }

        let requested_stores = string_array(arguments.get("stores"));
        let stores = if requested_stores.is_empty() {
            allowed.clone()
        } else {
            requested_stores
                .into_iter()
                .filter(|store| allowed.contains(store))
                .collect::<Vec<_>>()
        };

        if stores.is_empty() {
            return Ok(ToolOutput::error(
                "requested stores are not accessible at current trust level",
            ));
        }

        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(10)
            .min(50);

        let query = MemoryQuery {
            text: arguments
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            stores,
            types: string_array(arguments.get("types")),
            people: string_array(arguments.get("people")),
            after: millis_to_datetime(arguments.get("after_ms")),
            before: millis_to_datetime(arguments.get("before_ms")),
            limit,
            max_tokens: arguments
                .get("max_tokens")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok()),
        };

        let results = self.memory.search(&query).await?;

        let payload = serde_json::json!({
            "count": results.len(),
            "results": results,
        });

        debug!(count = results.len(), "memory_search complete");
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_timeline(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let anchor = arguments
            .get("anchor")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: anchor"))?;
        let before = arguments
            .get("before")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(3)
            .min(20);
        let after = arguments
            .get("after")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(3)
            .min(20);

        let allowed = accessible_stores(ctx.trust);
        let mut results = self.memory.timeline(anchor, before, after).await?;
        results.retain(|obs| allowed.contains(&obs.store));

        let payload = serde_json::json!({
            "count": results.len(),
            "results": results,
        });
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_get(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let ids = int64_array(arguments.get("ids"));
        if ids.is_empty() {
            return Ok(ToolOutput::error("missing required parameter: ids"));
        }

        let allowed = accessible_stores(ctx.trust);
        let mut observations = self.memory.get(&ids).await?;
        observations.retain(|obs| allowed.contains(&obs.store));

        let payload = serde_json::json!({
            "count": observations.len(),
            "observations": observations,
        });
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_write(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let title = arguments
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: title"))?
            .to_owned();

        let default_store = trust_to_store(ctx.trust).to_owned();
        let store = arguments
            .get("store")
            .and_then(Value::as_str)
            .unwrap_or(&default_store)
            .to_owned();

        let allowed = accessible_stores(ctx.trust);
        if !allowed.contains(&store) {
            return Ok(ToolOutput::error(format!(
                "store '{store}' is not accessible at current trust level"
            )));
        }

        let obs = NewObservation {
            session_key: arguments
                .get("session_key")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some(ctx.session_id.clone())),
            store: store.clone(),
            obs_type: arguments
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("discovery")
                .to_owned(),
            title,
            narrative: arguments
                .get("narrative")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            facts: string_array(arguments.get("facts")),
            tags: string_array(arguments.get("tags")),
            source: arguments
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("agent")
                .to_owned(),
            related_files: string_array(arguments.get("related_files")),
            related_people: string_array(arguments.get("related_people")),
            token_count: arguments
                .get("token_count")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok()),
            expires_at: millis_to_datetime(arguments.get("expires_at_ms")),
            min_trust: min_trust_for_store(&store),
        };

        let outcome = self.memory.write(obs).await?;

        let payload = match outcome {
            WriteOutcome::Added(id) => serde_json::json!({"outcome":"added","id":id}),
            WriteOutcome::Updated(id) => serde_json::json!({"outcome":"updated","id":id}),
            WriteOutcome::Deleted(id) => serde_json::json!({"outcome":"deleted","id":id}),
            WriteOutcome::Skipped => serde_json::json!({"outcome":"skipped"}),
            WriteOutcome::ExactDup => serde_json::json!({"outcome":"exact_dup"}),
        };

        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_history(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let observation_id = arguments
            .get("observation_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: observation_id"))?;

        let allowed = accessible_stores(ctx.trust);
        let observations = self.memory.get(&[observation_id]).await?;
        if observations.is_empty() {
            return Ok(ToolOutput::success("{\"count\":0,\"history\":[]}"));
        }

        if !allowed.contains(&observations[0].store) {
            return Ok(ToolOutput::error(
                "observation is not accessible at current trust level",
            ));
        }

        let history = self.memory.history(observation_id).await?;
        let payload = serde_json::json!({
            "count": history.len(),
            "history": history,
        });
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_people(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let allowed = accessible_stores(ctx.trust);
        let mut people = self.memory.people(query).await?;
        people.retain(|person| allowed.contains(&person.store));

        let payload = serde_json::json!({
            "count": people.len(),
            "people": people,
        });
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }
}

#[async_trait]
impl ToolExecutor for MemoryToolExecutor {
    async fn execute(&self, name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        match name {
            "memory_search" => self.exec_search(arguments, ctx).await,
            "memory_timeline" => self.exec_timeline(arguments, ctx).await,
            "memory_get" => self.exec_get(arguments, ctx).await,
            "memory_write" => self.exec_write(arguments, ctx).await,
            "memory_history" => self.exec_history(arguments, ctx).await,
            "memory_people" => self.exec_people(arguments, ctx).await,
            _ => Ok(ToolOutput::error(format!("unknown tool: {name}"))),
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        Self::defs()
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn int64_array(value: Option<&Value>) -> Vec<i64> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn millis_to_datetime(value: Option<&Value>) -> Option<chrono::DateTime<Utc>> {
    let millis = value.and_then(Value::as_i64)?;
    Utc.timestamp_millis_opt(millis).single()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::TrustLevel;
    use coop_memory::SqliteMemory;
    use std::path::PathBuf;

    fn ctx(trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: "coop:main".to_owned(),
            trust,
            workspace: PathBuf::from("."),
            user_name: None,
        }
    }

    fn executor() -> MemoryToolExecutor {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let memory = Arc::new(SqliteMemory::open(db_path, "coop").unwrap());

        // Leak tmpdir so the sqlite file remains available for the test lifetime.
        std::mem::forget(dir);
        MemoryToolExecutor::new(memory)
    }

    #[tokio::test]
    async fn write_rejects_private_store_for_inner_trust() {
        let exec = executor();
        let out = exec
            .execute(
                "memory_write",
                serde_json::json!({
                    "store": "private",
                    "title": "secret"
                }),
                &ctx(TrustLevel::Inner),
            )
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("not accessible"));
    }

    #[tokio::test]
    async fn search_returns_zero_for_public_trust() {
        let exec = executor();
        let write = exec
            .execute(
                "memory_write",
                serde_json::json!({
                    "store": "social",
                    "title": "public note",
                    "facts": ["hello"]
                }),
                &ctx(TrustLevel::Familiar),
            )
            .await
            .unwrap();
        assert!(!write.is_error);

        let out = exec
            .execute(
                "memory_search",
                serde_json::json!({"query": "public"}),
                &ctx(TrustLevel::Public),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("\"count\":0"));
    }
}
