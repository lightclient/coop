mod helpers;
mod maintenance;
mod query;
mod schema;
mod write_ops;

#[cfg(test)]
mod tests;

use anyhow::Result;
use async_trait::async_trait;
use coop_core::SessionKey;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, instrument, warn};

use crate::traits::{EmbeddingProvider, Memory, Reconciler};
use crate::types::{
    MemoryMaintenanceConfig, MemoryMaintenanceReport, MemoryQuery, NewObservation, Observation,
    ObservationHistoryEntry, ObservationIndex, Person, SessionSummary, WriteOutcome,
    embedding_text,
};

const DAY_MS: f32 = 86_400_000.0;

pub struct SqliteMemory {
    conn: Mutex<Connection>,
    agent_id: String,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    reconciler: Option<Arc<dyn Reconciler>>,
    vector_search_enabled: AtomicBool,
}

impl std::fmt::Debug for SqliteMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteMemory")
            .field("agent_id", &self.agent_id)
            .field("has_embedder", &self.embedder.is_some())
            .field("has_reconciler", &self.reconciler.is_some())
            .field(
                "vector_search_enabled",
                &self.vector_search_enabled.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct RawIndex {
    id: i64,
    title: String,
    obs_type: String,
    store: String,
    created_at: i64,
    updated_at: i64,
    token_count: u32,
    mention_count: u32,
    related_people: Vec<String>,
    fts_raw: f64,
}

impl SqliteMemory {
    pub fn open(path: impl AsRef<Path>, agent_id: impl Into<String>) -> Result<Self> {
        Self::open_with_components(path, agent_id, None, None)
    }

    pub fn open_with_embedder(
        path: impl AsRef<Path>,
        agent_id: impl Into<String>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<Self> {
        Self::open_with_components(path, agent_id, embedder, None)
    }

    pub fn open_with_components(
        path: impl AsRef<Path>,
        agent_id: impl Into<String>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        reconciler: Option<Arc<dyn Reconciler>>,
    ) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        schema::init_schema(&conn)?;
        let vec_enabled =
            schema::init_vector_schema(&conn, embedder.as_ref().map(|e| e.dimensions()));

        Ok(Self {
            conn: Mutex::new(conn),
            agent_id: agent_id.into(),
            embedder,
            reconciler,
            vector_search_enabled: AtomicBool::new(vec_enabled),
        })
    }

    async fn embed_text(&self, text: &str, reason: &'static str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?;
        debug!(reason, text_len = text.len(), "memory embedding request");

        match embedder.embed(text).await {
            Ok(embedding) => {
                debug!(
                    reason,
                    dimensions = embedding.len(),
                    "memory embedding complete"
                );
                Some(embedding)
            }
            Err(error) => {
                warn!(reason, error = %error, "memory embedding failed");
                None
            }
        }
    }

    pub(super) async fn embedding_for_query(&self, query: &MemoryQuery) -> Option<Vec<f32>> {
        let text = query.text.as_ref()?.trim();
        if text.is_empty() {
            return None;
        }

        self.embed_text(text, "search_query").await
    }

    pub(super) async fn embedding_for_observation(
        &self,
        title: &str,
        facts: &[String],
        reason: &'static str,
    ) -> Option<Vec<f32>> {
        let text = embedding_text(title, facts);
        if text.is_empty() {
            return None;
        }

        self.embed_text(&text, reason).await
    }

    pub(super) fn vector_search_enabled(&self) -> bool {
        self.vector_search_enabled.load(Ordering::Relaxed)
    }

    fn disable_vector_search(&self, error: &rusqlite::Error, context: &'static str) {
        if self.vector_search_enabled.swap(false, Ordering::Relaxed) {
            warn!(error = %error, context, "sqlite-vec path disabled, using FTS-only retrieval");
        }
    }

    pub(super) fn persist_embedding(
        &self,
        observation_id: i64,
        embedding: &[f32],
        now_ms: i64,
    ) -> Result<()> {
        let embedding_json = serde_json::to_string(embedding)?;
        let dimensions = i64::try_from(embedding.len()).unwrap_or(i64::MAX);

        let conn = self.conn.lock().expect("memory db mutex poisoned");
        conn.execute(
            "
                INSERT INTO observation_embeddings (observation_id, embedding, dimensions, updated_at)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(observation_id)
                DO UPDATE SET
                    embedding = excluded.embedding,
                    dimensions = excluded.dimensions,
                    updated_at = excluded.updated_at
                ",
            params![observation_id, embedding_json, dimensions, now_ms],
        )?;

        if self.vector_search_enabled()
            && let Err(error) = conn.execute(
                "INSERT OR REPLACE INTO observations_vec(rowid, embedding) VALUES (?, ?)",
                params![observation_id, serde_json::to_string(embedding)?],
            )
        {
            self.disable_vector_search(&error, "embedding_upsert");
        }

        drop(conn);
        Ok(())
    }

    pub(super) fn remove_embedding(&self, observation_id: i64) -> Result<()> {
        let conn = self.conn.lock().expect("memory db mutex poisoned");
        conn.execute(
            "DELETE FROM observation_embeddings WHERE observation_id = ?",
            params![observation_id],
        )?;

        if self.vector_search_enabled()
            && let Err(error) = conn.execute(
                "DELETE FROM observations_vec WHERE rowid = ?",
                params![observation_id],
            )
        {
            self.disable_vector_search(&error, "embedding_delete");
        }

        drop(conn);
        Ok(())
    }

    fn search_fts(
        &self,
        query: &MemoryQuery,
        now_ms: i64,
        fetch_limit: usize,
    ) -> Result<Vec<RawIndex>> {
        let mut sql = String::from(
            "
            SELECT
                o.id,
                o.title,
                o.type,
                o.store,
                o.created_at,
                o.updated_at,
                o.token_count,
                o.mention_count,
                o.related_people,
                bm25(observations_fts) AS fts_score
            FROM observations_fts
            JOIN observations o ON o.id = observations_fts.rowid
            WHERE o.agent_id = ?
              AND (o.expires_at IS NULL OR o.expires_at > ?)
            ",
        );

        let mut params: Vec<Value> = vec![Value::from(self.agent_id.clone()), Value::from(now_ms)];

        if let Some(text) = query.text.as_ref().filter(|t| !t.trim().is_empty()) {
            let fts_query = helpers::fts_query(text);
            if !fts_query.is_empty() {
                sql.push_str(" AND observations_fts MATCH ?");
                params.push(Value::from(fts_query));
            }
        }

        schema::append_filters_with_prefix(&mut sql, &mut params, query, "o.");

        sql.push_str(" ORDER BY fts_score LIMIT ?");
        params.push(Value::from(i64::try_from(fetch_limit).unwrap_or(i64::MAX)));

        let conn = self.conn.lock().expect("memory db mutex poisoned");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), helpers::raw_index_from_row)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        drop(stmt);
        drop(conn);
        Ok(out)
    }

    fn search_recent(
        &self,
        query: &MemoryQuery,
        now_ms: i64,
        fetch_limit: usize,
    ) -> Result<Vec<RawIndex>> {
        let mut sql = String::from(
            "
            SELECT
                id,
                title,
                type,
                store,
                created_at,
                updated_at,
                token_count,
                mention_count,
                related_people,
                0.0 AS fts_score
            FROM observations
            WHERE agent_id = ?
              AND (expires_at IS NULL OR expires_at > ?)
            ",
        );

        let mut params: Vec<Value> = vec![Value::from(self.agent_id.clone()), Value::from(now_ms)];

        schema::append_filters(&mut sql, &mut params, query);

        sql.push_str(" ORDER BY updated_at DESC LIMIT ?");
        params.push(Value::from(i64::try_from(fetch_limit).unwrap_or(i64::MAX)));

        let conn = self.conn.lock().expect("memory db mutex poisoned");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), helpers::raw_index_from_row)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        drop(stmt);
        drop(conn);
        Ok(out)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn search_vector(
        &self,
        query: &MemoryQuery,
        query_embedding: &[f32],
        now_ms: i64,
        fetch_limit: usize,
    ) -> Result<Vec<(RawIndex, f32)>> {
        if !self.vector_search_enabled() {
            debug!("sqlite-vec search disabled; using FTS-only retrieval");
            return Ok(Vec::new());
        }

        let mut sql = String::from(
            "
            SELECT
                o.id,
                o.title,
                o.type,
                o.store,
                o.created_at,
                o.updated_at,
                o.token_count,
                o.mention_count,
                o.related_people,
                0.0 AS fts_score,
                distance
            FROM observations_vec
            JOIN observations o ON o.id = observations_vec.rowid
            WHERE embedding MATCH ?
              AND k = ?
              AND o.agent_id = ?
              AND (o.expires_at IS NULL OR o.expires_at > ?)
            ",
        );

        let mut params: Vec<Value> = vec![
            Value::from(serde_json::to_string(query_embedding)?),
            Value::from(i64::try_from(fetch_limit).unwrap_or(i64::MAX)),
            Value::from(self.agent_id.clone()),
            Value::from(now_ms),
        ];

        schema::append_filters_with_prefix(&mut sql, &mut params, query, "o.");
        sql.push_str(" ORDER BY distance ASC LIMIT ?");
        params.push(Value::from(i64::try_from(fetch_limit).unwrap_or(i64::MAX)));

        let conn = self.conn.lock().expect("memory db mutex poisoned");
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(error) => {
                self.disable_vector_search(&error, "prepare_vector_query");
                return Ok(Vec::new());
            }
        };

        let rows = match stmt.query_map(params_from_iter(params), |row| {
            let raw = helpers::raw_index_from_row(row)?;
            let distance = row.get::<_, Option<f64>>(10)?.unwrap_or(0.0);
            let similarity = (1.0 / (1.0 + distance.max(0.0))) as f32;
            Ok((raw, similarity))
        }) {
            Ok(rows) => rows,
            Err(error) => {
                self.disable_vector_search(&error, "execute_vector_query");
                return Ok(Vec::new());
            }
        };

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        drop(stmt);
        drop(conn);
        Ok(out)
    }

    fn load_observation(&self, id: i64) -> Result<Option<Observation>> {
        let now_ms = helpers::now_ms();
        let conn = self.conn.lock().expect("memory db mutex poisoned");
        let mut stmt = conn.prepare(
            "
            SELECT
                id,
                title,
                COALESCE(narrative, ''),
                facts,
                tags,
                type,
                store,
                related_files,
                related_people,
                created_at,
                token_count,
                mention_count
            FROM observations
            WHERE id = ?
              AND agent_id = ?
              AND (expires_at IS NULL OR expires_at > ?)
            ",
        )?;

        let obs = stmt
            .query_row(
                params![id, self.agent_id, now_ms],
                helpers::observation_from_row,
            )
            .optional()?;
        drop(stmt);
        drop(conn);
        Ok(obs)
    }
}

#[async_trait]
impl Memory for SqliteMemory {
    #[instrument(skip(self, query), fields(limit = query.limit))]
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ObservationIndex>> {
        let query_embedding = self.embedding_for_query(query).await;
        query::search(self, query, query_embedding.as_deref())
    }

    #[instrument(skip(self))]
    async fn timeline(
        &self,
        anchor: i64,
        before: usize,
        after: usize,
    ) -> Result<Vec<ObservationIndex>> {
        query::timeline(self, anchor, before, after)
    }

    #[instrument(skip(self, ids), fields(count = ids.len()))]
    async fn get(&self, ids: &[i64]) -> Result<Vec<Observation>> {
        query::get(self, ids)
    }

    #[instrument(skip(self, obs), fields(obs_type = %obs.obs_type, store = %obs.store))]
    async fn write(&self, obs: NewObservation) -> Result<WriteOutcome> {
        write_ops::write(self, obs).await
    }

    #[instrument(skip(self))]
    async fn people(&self, query: &str) -> Result<Vec<Person>> {
        write_ops::people(self, query)
    }

    #[instrument(skip(self, session_key))]
    async fn summarize_session(&self, session_key: &SessionKey) -> Result<SessionSummary> {
        write_ops::summarize_session(self, session_key)
    }

    #[instrument(skip(self))]
    async fn history(&self, observation_id: i64) -> Result<Vec<ObservationHistoryEntry>> {
        write_ops::history(self, observation_id)
    }

    #[instrument(skip(self, config))]
    async fn run_maintenance(
        &self,
        config: &MemoryMaintenanceConfig,
    ) -> Result<MemoryMaintenanceReport> {
        maintenance::run(self, config)
    }
}
