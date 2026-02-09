use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

use crate::types::{MemoryQuery, Observation, ObservationIndex};

use super::{SqliteMemory, helpers};

pub(super) fn search(
    memory: &SqliteMemory,
    query: &MemoryQuery,
    query_embedding: Option<&[f32]>,
) -> Result<Vec<ObservationIndex>> {
    let now_ms = helpers::now_ms();
    let limit = if query.limit == 0 { 10 } else { query.limit };
    let fetch_limit = limit.max(1).saturating_mul(5);
    let query_has_text = query.text.as_ref().is_some_and(|t| !t.trim().is_empty());

    let mut rows = if query_has_text {
        memory.search_fts(query, now_ms, fetch_limit)?
    } else {
        memory.search_recent(query, now_ms, fetch_limit)?
    };

    let mut vector_scores = HashMap::new();
    if query_has_text {
        if let Some(embedding) = query_embedding {
            if memory.vector_search_enabled() {
                let vector_rows = memory.search_vector(query, embedding, now_ms, fetch_limit)?;
                let mut seen = rows.iter().map(|row| row.id).collect::<HashSet<_>>();

                for (row, similarity) in vector_rows {
                    vector_scores.insert(row.id, similarity);
                    if seen.insert(row.id) {
                        rows.push(row);
                    }
                }

                debug!(
                    vector_candidates = vector_scores.len(),
                    merged_candidates = rows.len(),
                    "memory vector candidate retrieval complete"
                );
            } else {
                info!("memory vector retrieval unavailable, using FTS-only path");
            }
        } else if memory.embedder.is_some() {
            warn!("memory query embedding missing, using FTS-only path");
        }
    }

    if !query.people.is_empty() {
        let people = query
            .people
            .iter()
            .map(|p| p.to_lowercase())
            .collect::<Vec<_>>();
        rows.retain(|row| {
            row.related_people
                .iter()
                .any(|person| people.contains(&person.to_lowercase()))
        });
    }

    rows.sort_by(|a, b| {
        helpers::score_row(b, now_ms, query_has_text, vector_scores.get(&b.id).copied())
            .partial_cmp(&helpers::score_row(
                a,
                now_ms,
                query_has_text,
                vector_scores.get(&a.id).copied(),
            ))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut total_tokens = 0usize;
    let mut out = Vec::new();

    for row in rows.into_iter().take(fetch_limit) {
        if out.len() >= limit {
            break;
        }

        let token_cost = usize::try_from(row.token_count).unwrap_or(0);
        if let Some(max_tokens) = query.max_tokens
            && !out.is_empty()
            && total_tokens.saturating_add(token_cost) > max_tokens
        {
            break;
        }

        let score = helpers::score_row(
            &row,
            now_ms,
            query_has_text,
            vector_scores.get(&row.id).copied(),
        );
        total_tokens = total_tokens.saturating_add(token_cost);
        out.push(helpers::to_index(row, score));
    }

    debug!(result_count = out.len(), "memory search complete");
    Ok(out)
}

#[allow(clippy::too_many_lines)]
pub(super) fn timeline(
    memory: &SqliteMemory,
    anchor: i64,
    before: usize,
    after: usize,
) -> Result<Vec<ObservationIndex>> {
    let now = helpers::now_ms();
    let conn = memory.conn.lock().expect("memory db mutex poisoned");

    let anchor_row: Option<super::RawIndex> = conn
        .query_row(
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
                WHERE id = ?
                  AND agent_id = ?
                  AND (expires_at IS NULL OR expires_at > ?)
                ",
            params![anchor, memory.agent_id, now],
            helpers::raw_index_from_row,
        )
        .optional()?;

    let Some(anchor_row) = anchor_row else {
        return Ok(Vec::new());
    };

    let mut before_stmt = conn.prepare(
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
              AND created_at < ?
              AND (expires_at IS NULL OR expires_at > ?)
            ORDER BY created_at DESC
            LIMIT ?
            ",
    )?;

    let before_rows = before_stmt.query_map(
        params![
            memory.agent_id,
            anchor_row.created_at,
            now,
            i64::try_from(before).unwrap_or(i64::MAX),
        ],
        helpers::raw_index_from_row,
    )?;

    let mut older = Vec::new();
    for row in before_rows {
        older.push(row?);
    }
    older.reverse();

    let mut after_stmt = conn.prepare(
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
              AND created_at > ?
              AND (expires_at IS NULL OR expires_at > ?)
            ORDER BY created_at ASC
            LIMIT ?
            ",
    )?;

    let after_rows = after_stmt.query_map(
        params![
            memory.agent_id,
            anchor_row.created_at,
            now,
            i64::try_from(after).unwrap_or(i64::MAX),
        ],
        helpers::raw_index_from_row,
    )?;

    let mut newer = Vec::new();
    for row in after_rows {
        newer.push(row?);
    }

    drop(after_stmt);
    drop(before_stmt);
    drop(conn);

    let all_rows = older
        .into_iter()
        .chain(std::iter::once(anchor_row))
        .chain(newer)
        .collect::<Vec<_>>();

    let timeline = all_rows
        .into_iter()
        .map(|row| helpers::to_index(row, 0.0))
        .collect();

    Ok(timeline)
}

pub(super) fn get(memory: &SqliteMemory, ids: &[i64]) -> Result<Vec<Observation>> {
    let mut by_id = HashMap::new();
    for id in ids {
        if let Some(obs) = memory.load_observation(*id)? {
            by_id.insert(*id, obs);
        }
    }

    let mut out = Vec::new();
    for id in ids {
        if let Some(obs) = by_id.remove(id) {
            out.push(obs);
        }
    }
    Ok(out)
}
