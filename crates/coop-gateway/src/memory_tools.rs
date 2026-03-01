use anyhow::Result;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use coop_core::TrustLevel;
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput};
use coop_memory::{
    Memory, MemoryQuery, NewObservation, WriteOutcome, accessible_stores, min_trust_for_store,
    normalize_file_path, trust_to_store,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
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

    #[allow(clippy::too_many_lines)]
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
                        "file": {
                            "type": "string",
                            "description": "Filter to observations referencing this file path"
                        },
                        "after_ms": { "type": "integer" },
                        "before_ms": { "type": "integer" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                        "max_tokens": { "type": "integer", "minimum": 1 }
                    }
                }),
            ),
            ToolDef::new(
                "memory_files",
                "Find observations linked to a file path. Supports exact match or directory prefix.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to search (for example: 'src/main.rs' or 'crates/coop-gateway/')"
                        },
                        "prefix": {
                            "type": "boolean",
                            "description": "If true, match all files under this directory prefix"
                        },
                        "limit": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 50
                        },
                        "check_exists": {
                            "type": "boolean",
                            "description": "If true, check which referenced files still exist on disk"
                        }
                    },
                    "required": ["path"]
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
            ToolDef::new(
                "memory_sessions",
                "List recent session summaries showing what was discussed and completed.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer", "minimum": 1, "maximum": 50 }
                    }
                }),
            ),
            ToolDef::new(
                "memory_alias",
                "Add an alias for a known person. Use when you learn someone's nickname, \
                 abbreviation, or alternative name. Aliases are searchable via memory_people.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact canonical name of the person (as shown in memory_people)"
                        },
                        "alias": {
                            "type": "string",
                            "description": "The alternative name, nickname, or abbreviation to add"
                        }
                    },
                    "required": ["name", "alias"]
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

        let file_filter = arguments
            .get("file")
            .and_then(Value::as_str)
            .map(normalize_file_path)
            .filter(|path| !path.is_empty());

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

        let mut results = self.memory.search(&query).await?;

        if let Some(path) = file_filter.as_ref() {
            let ids = results.iter().map(|result| result.id).collect::<Vec<_>>();
            let observations = self.memory.get(&ids).await?;

            let matching_ids = observations
                .into_iter()
                .filter(|obs| {
                    obs.related_files
                        .iter()
                        .map(|file| normalize_file_path(file))
                        .any(|file| file == *path)
                })
                .map(|obs| obs.id)
                .collect::<HashSet<_>>();

            results.retain(|result| matching_ids.contains(&result.id));
        }

        let result_count = results.len();
        let payload = serde_json::json!({
            "count": result_count,
            "results": results,
        });

        debug!(
            count = result_count,
            has_file_filter = file_filter.is_some(),
            "memory_search complete"
        );
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_files(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let raw_path = arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing required parameter: path"))?;

        let path = normalize_file_path(raw_path);
        if path.is_empty() {
            return Ok(ToolOutput::error("path cannot be empty"));
        }

        let prefix = arguments
            .get("prefix")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let check_exists = arguments
            .get("check_exists")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(10)
            .clamp(1, 50);

        let allowed = accessible_stores(ctx.trust);
        if allowed.is_empty() {
            let payload = serde_json::json!({
                "count": 0,
                "path": path,
                "results": [],
            });
            return Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?));
        }

        let mut results = self.memory.search_by_file(&path, prefix, limit).await?;
        results.retain(|row| allowed.contains(&row.store));

        let ids = results.iter().map(|row| row.id).collect::<Vec<_>>();
        let observations = self.memory.get(&ids).await?;
        let related_files_by_id = observations
            .into_iter()
            .map(|obs| (obs.id, normalize_file_list(obs.related_files)))
            .collect::<HashMap<_, _>>();

        let mut stale_file_count = 0usize;
        let mut rendered_results = Vec::with_capacity(results.len());

        for result in results {
            let files = related_files_by_id
                .get(&result.id)
                .cloned()
                .unwrap_or_default();

            let rendered_files = files
                .into_iter()
                .map(|file| {
                    if check_exists {
                        let exists = file_exists_in_workspace(&ctx.workspace, &file);
                        if !exists {
                            stale_file_count = stale_file_count.saturating_add(1);
                        }
                        serde_json::json!({"path": file, "exists": exists})
                    } else {
                        serde_json::json!({"path": file})
                    }
                })
                .collect::<Vec<_>>();

            rendered_results.push(serde_json::json!({
                "id": result.id,
                "title": result.title,
                "type": result.obs_type,
                "store": result.store,
                "created": result.created_at.format("%Y-%m-%d").to_string(),
                "files": rendered_files,
            }));
        }

        debug!(
            path = %path,
            prefix,
            result_count = rendered_results.len(),
            stale_file_count,
            "memory_files complete"
        );

        let payload = serde_json::json!({
            "count": rendered_results.len(),
            "path": path,
            "results": rendered_results,
        });

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
            related_files: normalize_file_list(string_array(arguments.get("related_files"))),
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

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_alias(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Inner {
            return Ok(ToolOutput::error(
                "memory_alias requires at least inner trust",
            ));
        }

        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let alias = arguments
            .get("alias")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if name.is_empty() || alias.is_empty() {
            return Ok(ToolOutput::error("both name and alias are required"));
        }

        let added = self.memory.add_person_alias(name, alias).await?;
        let payload = if added {
            serde_json::json!({ "status": "added", "person": name, "alias": alias })
        } else {
            serde_json::json!({ "status": "no_change", "reason": "person not found or alias already exists" })
        };
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }

    #[instrument(skip(self, arguments, ctx))]
    async fn exec_sessions(&self, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        if ctx.trust > TrustLevel::Full {
            return Ok(ToolOutput::success("{\"count\":0,\"sessions\":[]}"));
        }

        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(10)
            .clamp(1, 50);

        let sessions = self.memory.recent_session_summaries(limit).await?;
        let payload = serde_json::json!({
            "count": sessions.len(),
            "sessions": sessions,
        });
        Ok(ToolOutput::success(serde_json::to_string_pretty(&payload)?))
    }
}

#[async_trait]
impl ToolExecutor for MemoryToolExecutor {
    async fn execute(&self, name: &str, arguments: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        match name {
            "memory_search" => self.exec_search(arguments, ctx).await,
            "memory_files" => self.exec_files(arguments, ctx).await,
            "memory_timeline" => self.exec_timeline(arguments, ctx).await,
            "memory_get" => self.exec_get(arguments, ctx).await,
            "memory_write" => self.exec_write(arguments, ctx).await,
            "memory_history" => self.exec_history(arguments, ctx).await,
            "memory_people" => self.exec_people(arguments, ctx).await,
            "memory_alias" => self.exec_alias(arguments, ctx).await,
            "memory_sessions" => self.exec_sessions(arguments, ctx).await,
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

fn normalize_file_list(files: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for file in files {
        let path = normalize_file_path(&file);
        if path.is_empty() || normalized.contains(&path) {
            continue;
        }
        normalized.push(path);
    }
    normalized
}

fn file_exists_in_workspace(workspace: &Path, file: &str) -> bool {
    let path = Path::new(file);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    resolved.exists()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use coop_core::{SessionKey, SessionKind, TrustLevel};
    use coop_memory::SqliteMemory;
    use std::path::Path;

    fn ctx(trust: TrustLevel) -> ToolContext {
        ctx_in(trust, Path::new("."))
    }

    fn ctx_in(trust: TrustLevel, workspace: &Path) -> ToolContext {
        ToolContext {
            session_id: "coop:main".to_owned(),
            trust,
            workspace: workspace.to_path_buf(),
            user_name: None,
        }
    }

    fn executor_with_memory() -> (MemoryToolExecutor, Arc<SqliteMemory>) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let memory = Arc::new(SqliteMemory::open(db_path, "coop").unwrap());

        // Leak tmpdir so the sqlite file remains available for the test lifetime.
        std::mem::forget(dir);

        let executor = MemoryToolExecutor::new(Arc::clone(&memory) as Arc<dyn Memory>);
        (executor, memory)
    }

    fn executor() -> MemoryToolExecutor {
        executor_with_memory().0
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

    #[tokio::test]
    async fn memory_sessions_returns_recent_summaries() {
        let (exec, memory) = executor_with_memory();

        memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: "shared".to_owned(),
                obs_type: "decision".to_owned(),
                title: "Use SQLite for memory".to_owned(),
                narrative: "Persist memory in sqlite".to_owned(),
                facts: vec!["SQLite enabled".to_owned()],
                tags: vec!["memory".to_owned()],
                source: "test".to_owned(),
                related_files: vec!["crates/coop-memory/src/sqlite/mod.rs".to_owned()],
                related_people: vec!["alice".to_owned()],
                token_count: Some(32),
                expires_at: None,
                min_trust: min_trust_for_store("shared"),
            })
            .await
            .unwrap();

        memory
            .summarize_session(&SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Main,
            })
            .await
            .unwrap();

        let out = exec
            .execute(
                "memory_sessions",
                serde_json::json!({"limit": 5}),
                &ctx(TrustLevel::Full),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("\"count\": 1"));
        assert!(out.content.contains("Use SQLite for memory"));
    }

    #[tokio::test]
    async fn memory_search_file_filter_matches_related_file() {
        let (exec, memory) = executor_with_memory();

        let first = memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: "shared".to_owned(),
                obs_type: "technical".to_owned(),
                title: "match first".to_owned(),
                narrative: String::new(),
                facts: vec!["alpha".to_owned()],
                tags: vec![],
                source: "test".to_owned(),
                related_files: vec!["./src//main.rs".to_owned()],
                related_people: vec![],
                token_count: Some(8),
                expires_at: None,
                min_trust: min_trust_for_store("shared"),
            })
            .await
            .unwrap();
        let WriteOutcome::Added(first_id) = first else {
            panic!("expected add");
        };

        memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: "shared".to_owned(),
                obs_type: "technical".to_owned(),
                title: "match second".to_owned(),
                narrative: String::new(),
                facts: vec!["beta".to_owned()],
                tags: vec![],
                source: "test".to_owned(),
                related_files: vec!["src/lib.rs".to_owned()],
                related_people: vec![],
                token_count: Some(8),
                expires_at: None,
                min_trust: min_trust_for_store("shared"),
            })
            .await
            .unwrap();

        let out = exec
            .execute(
                "memory_search",
                serde_json::json!({
                    "query": "match",
                    "file": "src/main.rs"
                }),
                &ctx(TrustLevel::Inner),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        let payload: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["results"][0]["id"], first_id);
    }

    #[tokio::test]
    async fn memory_files_respects_trust_gating() {
        let (exec, memory) = executor_with_memory();

        memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: "private".to_owned(),
                obs_type: "decision".to_owned(),
                title: "private note".to_owned(),
                narrative: String::new(),
                facts: vec![],
                tags: vec![],
                source: "test".to_owned(),
                related_files: vec!["src/main.rs".to_owned()],
                related_people: vec![],
                token_count: Some(8),
                expires_at: None,
                min_trust: min_trust_for_store("private"),
            })
            .await
            .unwrap();

        memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: "social".to_owned(),
                obs_type: "decision".to_owned(),
                title: "social note".to_owned(),
                narrative: String::new(),
                facts: vec![],
                tags: vec![],
                source: "test".to_owned(),
                related_files: vec!["src/main.rs".to_owned()],
                related_people: vec![],
                token_count: Some(8),
                expires_at: None,
                min_trust: min_trust_for_store("social"),
            })
            .await
            .unwrap();

        let out = exec
            .execute(
                "memory_files",
                serde_json::json!({"path": "src/main.rs"}),
                &ctx(TrustLevel::Familiar),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        let payload: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(payload["count"], 1);
        assert_eq!(payload["results"][0]["store"], "social");
    }

    #[tokio::test]
    async fn memory_files_check_exists_reports_flags() {
        let (exec, memory) = executor_with_memory();
        let dir = tempfile::tempdir().unwrap();
        let existing_file = dir.path().join("existing.rs");
        std::fs::write(&existing_file, "fn main() {}\n").unwrap();

        memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: "shared".to_owned(),
                obs_type: "technical".to_owned(),
                title: "file state".to_owned(),
                narrative: String::new(),
                facts: vec![],
                tags: vec![],
                source: "test".to_owned(),
                related_files: vec!["existing.rs".to_owned(), "missing.rs".to_owned()],
                related_people: vec![],
                token_count: Some(8),
                expires_at: None,
                min_trust: min_trust_for_store("shared"),
            })
            .await
            .unwrap();

        let out = exec
            .execute(
                "memory_files",
                serde_json::json!({
                    "path": "existing.rs",
                    "check_exists": true
                }),
                &ctx_in(TrustLevel::Inner, dir.path()),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        let payload: Value = serde_json::from_str(&out.content).unwrap();

        let files = payload["results"][0]["files"].as_array().unwrap();
        let existing = files
            .iter()
            .find(|row| row["path"] == "existing.rs")
            .unwrap();
        let missing = files
            .iter()
            .find(|row| row["path"] == "missing.rs")
            .unwrap();

        assert_eq!(existing["exists"], true);
        assert_eq!(missing["exists"], false);
    }
}
