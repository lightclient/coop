use chrono::{DateTime, TimeZone, Utc};
use rusqlite::Row;
use sha2::{Digest, Sha256};

use crate::types::{Observation, ObservationIndex};

use super::{DAY_MS, RawIndex};

pub(super) fn raw_index_from_row(row: &Row<'_>) -> rusqlite::Result<RawIndex> {
    let related_people: String = row.get(8)?;
    let related_people = from_json(&related_people);

    Ok(RawIndex {
        id: row.get(0)?,
        title: row.get(1)?,
        obs_type: row.get(2)?,
        store: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        token_count: row.get::<_, Option<u32>>(6)?.unwrap_or(0),
        mention_count: row.get::<_, Option<u32>>(7)?.unwrap_or(0),
        related_people,
        fts_raw: row.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
    })
}

pub(super) fn observation_from_row(row: &Row<'_>) -> rusqlite::Result<Observation> {
    let facts: String = row.get(3)?;
    let tags: String = row.get(4)?;
    let files: String = row.get(7)?;
    let people: String = row.get(8)?;

    Ok(Observation {
        id: row.get(0)?,
        title: row.get(1)?,
        narrative: row.get(2)?,
        facts: from_json(&facts),
        tags: from_json(&tags),
        obs_type: row.get(5)?,
        store: row.get(6)?,
        related_files: from_json(&files),
        related_people: from_json(&people),
        created_at: dt_from_ms(row.get(9)?),
        token_count: row.get::<_, Option<u32>>(10)?.unwrap_or(0),
        mention_count: row.get::<_, Option<u32>>(11)?.unwrap_or(0),
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(super) fn score_row(
    row: &RawIndex,
    now_ms: i64,
    has_text_query: bool,
    vector_similarity: Option<f32>,
) -> f32 {
    let recency_days = ((now_ms - row.updated_at).max(0) as f32) / DAY_MS;
    let recency_score = 1.0 / (1.0 + recency_days);
    let mention_score = (1.0 + row.mention_count as f32).ln() / (1.0_f32 + 10.0).ln();

    if has_text_query {
        let fts_score = 1.0 / (1.0 + row.fts_raw.abs() as f32);
        let vector_score = vector_similarity.unwrap_or(0.0).clamp(0.0, 1.0);
        0.45 * fts_score + 0.25 * vector_score + 0.15 * recency_score + 0.15 * mention_score
    } else {
        0.7 * recency_score + 0.3 * mention_score
    }
}

pub(super) fn to_index(row: RawIndex, score: f32) -> ObservationIndex {
    ObservationIndex {
        id: row.id,
        title: row.title,
        obs_type: row.obs_type,
        store: row.store,
        created_at: dt_from_ms(row.created_at),
        token_count: row.token_count,
        mention_count: row.mention_count,
        score,
        related_people: row.related_people,
    }
}

pub(super) fn observation_hash(title: &str, facts: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update([0]);
    for fact in facts {
        hasher.update(fact.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    hex::encode(digest)
}

pub(super) fn to_json(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
}

/// Build an FTS5 query from space-separated terms.
///
/// Uses OR matching so partial overlap still returns results. BM25 ranking
/// naturally scores docs with more matching terms higher — no strict AND
/// required.
pub(super) fn fts_query(text: &str) -> String {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"", token.replace('"', " ")))
        .collect();

    tokens.join(" OR ")
}

pub(super) fn from_json(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub(super) fn ms_from_dt(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

pub(super) fn dt_from_ms(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

pub(super) fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}
