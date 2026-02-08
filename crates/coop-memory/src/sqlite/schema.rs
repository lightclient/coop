use anyhow::Result;
use rusqlite::{Connection, types::Value};

use crate::types::MemoryQuery;

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
            observation_id INTEGER NOT NULL REFERENCES observations(id),
            old_title TEXT,
            old_facts TEXT,
            new_title TEXT,
            new_facts TEXT,
            event TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_history_obs ON observation_history(observation_id);

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

pub(super) fn append_filters(sql: &mut String, params: &mut Vec<Value>, query: &MemoryQuery) {
    if !query.stores.is_empty() {
        sql.push_str(" AND store IN (");
        append_placeholders(sql, query.stores.len());
        sql.push(')');
        params.extend(query.stores.iter().cloned().map(Value::from));
    }

    if !query.types.is_empty() {
        sql.push_str(" AND type IN (");
        append_placeholders(sql, query.types.len());
        sql.push(')');
        params.extend(query.types.iter().cloned().map(Value::from));
    }

    if let Some(after) = query.after {
        sql.push_str(" AND created_at >= ?");
        params.push(Value::from(after.timestamp_millis()));
    }

    if let Some(before) = query.before {
        sql.push_str(" AND created_at <= ?");
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
