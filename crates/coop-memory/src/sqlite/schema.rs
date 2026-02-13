use std::sync::Once;

use anyhow::Result;
use rusqlite::{Connection, types::Value};
use tracing::warn;

use crate::types::MemoryQuery;

static SQLITE_VEC_REGISTERED: Once = Once::new();

/// Register sqlite-vec as an auto-extension so every new connection gets it.
/// Safe to call multiple times — the registration only happens once.
///
/// # Safety justification
/// `sqlite3_vec_init` has the standard SQLite extension entry-point signature
/// `(sqlite3*, char**, const sqlite3_api_routines*) -> int`. The transmute
/// casts it to the `Option<fn()>` that `sqlite3_auto_extension` expects —
/// SQLite internally casts it back before calling. This is the pattern the
/// `sqlite-vec` crate itself uses in its own tests.
#[allow(unsafe_code)]
pub(super) fn ensure_sqlite_vec_registered() {
    SQLITE_VEC_REGISTERED.call_once(|| {
        // SAFETY: see doc comment above. sqlite3_vec_init is a well-known
        // C entry point compiled from sqlite-vec.c with SQLITE_CORE.
        unsafe {
            type AutoExtFn = unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int;

            let func: AutoExtFn = std::mem::transmute::<*const (), AutoExtFn>(
                sqlite_vec::sqlite3_vec_init as *const (),
            );
            let rc = rusqlite::ffi::sqlite3_auto_extension(Some(func));
            if rc != rusqlite::ffi::SQLITE_OK {
                warn!(rc, "failed to register sqlite-vec auto-extension");
            }
        }
    });
}

#[allow(clippy::too_many_lines)]
pub(super) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS observations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            session_key TEXT,
            store TEXT NOT NULL,
            type TEXT NOT NULL,
            title TEXT NOT NULL,
            narrative TEXT,
            facts TEXT NOT NULL DEFAULT '[]',
            tags TEXT NOT NULL DEFAULT '[]',
            source TEXT,
            related_files TEXT NOT NULL DEFAULT '[]',
            related_people TEXT NOT NULL DEFAULT '[]',
            hash TEXT NOT NULL,
            mention_count INTEGER NOT NULL DEFAULT 1,
            token_count INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            expires_at INTEGER,
            min_trust TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_obs_agent ON observations(agent_id);
        CREATE INDEX IF NOT EXISTS idx_obs_store ON observations(store);
        CREATE INDEX IF NOT EXISTS idx_obs_type ON observations(type);
        CREATE INDEX IF NOT EXISTS idx_obs_created ON observations(created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_obs_trust ON observations(min_trust);
        CREATE INDEX IF NOT EXISTS idx_obs_hash ON observations(agent_id, hash);

        CREATE TABLE IF NOT EXISTS observation_embeddings (
            observation_id INTEGER PRIMARY KEY,
            embedding TEXT NOT NULL,
            dimensions INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY(observation_id) REFERENCES observations(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_obs_embedding_dims ON observation_embeddings(dimensions);

        CREATE VIRTUAL TABLE IF NOT EXISTS observations_fts USING fts5(
            title,
            narrative,
            facts,
            tags,
            content='observations',
            content_rowid='id'
        );

        CREATE TRIGGER IF NOT EXISTS observations_ai AFTER INSERT ON observations BEGIN
            INSERT INTO observations_fts(rowid, title, narrative, facts, tags)
            VALUES (new.id, new.title, COALESCE(new.narrative, ''), new.facts, new.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS observations_ad AFTER DELETE ON observations BEGIN
            INSERT INTO observations_fts(observations_fts, rowid, title, narrative, facts, tags)
            VALUES ('delete', old.id, old.title, COALESCE(old.narrative, ''), old.facts, old.tags);
        END;

        CREATE TRIGGER IF NOT EXISTS observations_au AFTER UPDATE ON observations BEGIN
            INSERT INTO observations_fts(observations_fts, rowid, title, narrative, facts, tags)
            VALUES ('delete', old.id, old.title, COALESCE(old.narrative, ''), old.facts, old.tags);
            INSERT INTO observations_fts(rowid, title, narrative, facts, tags)
            VALUES (new.id, new.title, COALESCE(new.narrative, ''), new.facts, new.tags);
        END;

        CREATE TABLE IF NOT EXISTS observation_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            observation_id INTEGER NOT NULL REFERENCES observations(id) ON DELETE CASCADE,
            old_title TEXT,
            old_facts TEXT,
            new_title TEXT,
            new_facts TEXT,
            event TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_history_obs ON observation_history(observation_id);

        CREATE TABLE IF NOT EXISTS observation_archive (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            original_observation_id INTEGER NOT NULL,
            agent_id TEXT NOT NULL,
            session_key TEXT,
            store TEXT NOT NULL,
            type TEXT NOT NULL,
            title TEXT NOT NULL,
            narrative TEXT,
            facts TEXT NOT NULL,
            tags TEXT NOT NULL,
            source TEXT,
            related_files TEXT NOT NULL,
            related_people TEXT NOT NULL,
            hash TEXT NOT NULL,
            mention_count INTEGER NOT NULL,
            token_count INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            expires_at INTEGER,
            min_trust TEXT NOT NULL,
            archived_at INTEGER NOT NULL,
            archive_reason TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_obs_archive_agent_created
            ON observation_archive(agent_id, archived_at DESC);
        CREATE INDEX IF NOT EXISTS idx_obs_archive_original
            ON observation_archive(agent_id, original_observation_id);

        CREATE TABLE IF NOT EXISTS session_summaries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            session_key TEXT NOT NULL,
            request TEXT,
            outcome TEXT,
            decisions TEXT,
            open_items TEXT,
            observation_count INTEGER,
            created_at INTEGER NOT NULL
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_session_summaries_key
            ON session_summaries(agent_id, session_key);

        CREATE TABLE IF NOT EXISTS people (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            name TEXT NOT NULL,
            store TEXT NOT NULL,
            facts TEXT,
            last_mentioned INTEGER,
            mention_count INTEGER DEFAULT 0,
            UNIQUE(agent_id, name)
        );
        ",
    )?;

    Ok(())
}

pub(super) fn init_vector_schema(conn: &Connection, dimensions: Option<usize>) -> bool {
    let Some(dimensions) = dimensions else {
        return false;
    };

    let sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS observations_vec USING vec0(embedding float[{dimensions}]);"
    );

    match conn.execute_batch(&sql) {
        Ok(()) => true,
        Err(error) => {
            warn!(
                error = %error,
                dimensions,
                "sqlite-vec unavailable, falling back to FTS-only retrieval"
            );
            false
        }
    }
}

pub(super) fn append_filters(sql: &mut String, params: &mut Vec<Value>, query: &MemoryQuery) {
    append_filters_with_prefix(sql, params, query, "");
}

pub(super) fn append_filters_with_prefix(
    sql: &mut String,
    params: &mut Vec<Value>,
    query: &MemoryQuery,
    prefix: &str,
) {
    if !query.stores.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(prefix);
        sql.push_str("store IN (");
        append_placeholders(sql, query.stores.len());
        sql.push(')');
        params.extend(query.stores.iter().cloned().map(Value::from));
    }

    if !query.types.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(prefix);
        sql.push_str("type IN (");
        append_placeholders(sql, query.types.len());
        sql.push(')');
        params.extend(query.types.iter().cloned().map(Value::from));
    }

    if let Some(after) = query.after {
        sql.push_str(" AND ");
        sql.push_str(prefix);
        sql.push_str("created_at >= ?");
        params.push(Value::from(after.timestamp_millis()));
    }

    if let Some(before) = query.before {
        sql.push_str(" AND ");
        sql.push_str(prefix);
        sql.push_str("created_at <= ?");
        params.push(Value::from(before.timestamp_millis()));
    }
}

fn append_placeholders(sql: &mut String, count: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
}
