mod helpers;
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
use std::sync::{Arc, Mutex};
use tracing::instrument;

use crate::traits::{EmbeddingProvider, Memory};
use crate::types::{
    MemoryQuery, NewObservation, Observation, ObservationHistoryEntry, ObservationIndex, Person,
    SessionSummary, WriteOutcome,
};

const DAY_MS: f32 = 86_400_000.0;

pub struct SqliteMemory {
    conn: Mutex<Connection>,
    agent_id: String,
    #[allow(dead_code)]
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

impl std::fmt::Debug for SqliteMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteMemory")
            .field("agent_id", &self.agent_id)
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
        Self::open_with_embedder(path, agent_id, None)
    }

    pub fn open_with_embedder(
        path: impl AsRef<Path>,
        agent_id: impl Into<String>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
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

        Ok(Self {
            conn: Mutex::new(conn),
            agent_id: agent_id.into(),
            embedder,
        })
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
            sql.push_str(" AND observations_fts MATCH ?");
            params.push(Value::from(text.clone()));
        }

        schema::append_filters(&mut sql, &mut params, query);

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
        query::search(self, query)
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
        write_ops::write(self, obs)
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
}
