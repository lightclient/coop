//! Session transcript indexing and FTS5 search.

use anyhow::Result;
use rusqlite::params;
use tracing::{debug, instrument};

use super::SqliteMemory;
use super::helpers;
use crate::types::{SessionMessage, SessionSearchHit};

impl SqliteMemory {
    #[instrument(skip(self, msg), fields(session = %msg.session_key, role = %msg.role))]
    pub(super) fn index_session_message_sync(&self, msg: &SessionMessage) -> Result<()> {
        let content = msg.content.trim();
        if content.is_empty() {
            return Ok(());
        }

        let created_at = helpers::ms_from_dt(msg.created_at);
        let conn = self.conn.lock().expect("memory db mutex poisoned");
        conn.execute(
            "INSERT INTO session_messages (agent_id, session_key, role, content, tool_name, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                self.agent_id,
                msg.session_key,
                msg.role,
                content,
                msg.tool_name,
                created_at,
            ],
        )?;
        drop(conn);
        Ok(())
    }

    /// FTS5 search across session transcripts, grouped by session.
    ///
    /// Returns the top `limit` sessions ranked by FTS relevance, with a
    /// snippet from the best-matching message.
    ///
    /// `exclude_since` skips messages newer than the given timestamp so
    /// the current turn's own messages don't appear in results while
    /// older messages from the same session key remain searchable.
    #[instrument(skip(self), fields(query_len = query.len(), limit))]
    pub(super) fn search_session_messages_sync(
        &self,
        query: &str,
        limit: usize,
        exclude_since: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<SessionSearchHit>> {
        let fts_query = helpers::fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let limit_i64 = i64::try_from(limit.clamp(1, 20)).unwrap_or(10);

        // Build query with optional recency exclusion.
        // We use the FTS5 `rank` hidden column (not bm25()) because rank
        // works with GROUP BY while bm25() does not.
        let (sql, use_cutoff) = if exclude_since.is_some() {
            (
                "SELECT
                    sm.session_key,
                    substr(sm.content, 1, 200) AS snippet,
                    COUNT(*) AS match_count,
                    MIN(sm.created_at) AS earliest,
                    MAX(sm.created_at) AS latest,
                    MIN(session_messages_fts.rank) AS best_score
                FROM session_messages_fts
                JOIN session_messages sm ON sm.id = session_messages_fts.rowid
                WHERE session_messages_fts MATCH ?
                  AND sm.agent_id = ?
                  AND sm.created_at < ?
                GROUP BY sm.session_key
                ORDER BY best_score ASC
                LIMIT ?"
                    .to_owned(),
                true,
            )
        } else {
            (
                "SELECT
                    sm.session_key,
                    substr(sm.content, 1, 200) AS snippet,
                    COUNT(*) AS match_count,
                    MIN(sm.created_at) AS earliest,
                    MAX(sm.created_at) AS latest,
                    MIN(session_messages_fts.rank) AS best_score
                FROM session_messages_fts
                JOIN session_messages sm ON sm.id = session_messages_fts.rowid
                WHERE session_messages_fts MATCH ?
                  AND sm.agent_id = ?
                GROUP BY sm.session_key
                ORDER BY best_score ASC
                LIMIT ?"
                    .to_owned(),
                false,
            )
        };

        let conn = self.conn.lock().expect("memory db mutex poisoned");
        let mut stmt = conn.prepare(&sql)?;

        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(SessionSearchHit {
                session_key: row.get(0)?,
                snippet: row.get(1)?,
                message_count: usize::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                earliest: helpers::dt_from_ms(row.get(3)?),
                latest: helpers::dt_from_ms(row.get(4)?),
                score: row.get(5)?,
            })
        };

        let mut hits = Vec::new();
        if use_cutoff {
            let cutoff_ms = helpers::ms_from_dt(exclude_since.expect("use_cutoff is true"));
            let rows = stmt.query_map(
                params![fts_query, self.agent_id, cutoff_ms, limit_i64],
                map_row,
            )?;
            for row in rows {
                hits.push(row?);
            }
        } else {
            let rows = stmt.query_map(params![fts_query, self.agent_id, limit_i64], map_row)?;
            for row in rows {
                hits.push(row?);
            }
        }
        drop(stmt);
        drop(conn);

        debug!(result_count = hits.len(), "session search complete");
        Ok(hits)
    }

    /// Load messages for a single session from the search index.
    #[instrument(skip(self), fields(session_key, limit))]
    pub(super) fn load_session_messages_sync(
        &self,
        session_key: &str,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        let limit_i64 = i64::try_from(limit.max(1)).unwrap_or(500);
        let conn = self.conn.lock().expect("memory db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT session_key, role, content, tool_name, created_at
             FROM session_messages
             WHERE agent_id = ? AND session_key = ?
             ORDER BY created_at ASC
             LIMIT ?",
        )?;

        let rows = stmt.query_map(params![self.agent_id, session_key, limit_i64], |row| {
            Ok(SessionMessage {
                session_key: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                tool_name: row.get(3)?,
                created_at: helpers::dt_from_ms(row.get(4)?),
            })
        })?;

        let mut msgs = Vec::new();
        for row in rows {
            msgs.push(row?);
        }
        drop(stmt);
        drop(conn);
        Ok(msgs)
    }
}
