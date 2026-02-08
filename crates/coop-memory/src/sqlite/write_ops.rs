use anyhow::Result;
use coop_core::{SessionKey, prompt::count_tokens};
use rusqlite::{OptionalExtension, params};

use crate::types::{
    NewObservation, ObservationHistoryEntry, Person, SessionSummary, WriteOutcome, trust_to_str,
};

use super::{SqliteMemory, helpers};

#[allow(clippy::too_many_lines)]
pub(super) fn write(memory: &SqliteMemory, obs: NewObservation) -> Result<WriteOutcome> {
    let now = helpers::now_ms();
    let hash = helpers::observation_hash(&obs.title, &obs.facts);

    let token_count = obs.token_count.unwrap_or_else(|| {
        let mut text = obs.title.clone();
        if !obs.narrative.is_empty() {
            text.push(' ');
            text.push_str(&obs.narrative);
        }
        if !obs.facts.is_empty() {
            text.push(' ');
            text.push_str(&obs.facts.join("; "));
        }
        u32::try_from(count_tokens(&text)).unwrap_or(u32::MAX)
    });

    let facts_json = helpers::to_json(&obs.facts);
    let tags_json = helpers::to_json(&obs.tags);
    let files_json = helpers::to_json(&obs.related_files);
    let people_json = helpers::to_json(&obs.related_people);
    let expires_at = obs.expires_at.map(helpers::ms_from_dt);

    let conn = memory.conn.lock().expect("memory db mutex poisoned");

    let exact_dup: Option<(i64, u32)> = conn
        .query_row(
            "
                SELECT id, mention_count
                FROM observations
                WHERE agent_id = ?
                  AND hash = ?
                  AND (expires_at IS NULL OR expires_at > ?)
                ",
            params![memory.agent_id, hash, now],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    if let Some((id, mention_count)) = exact_dup {
        conn.execute(
            "
                UPDATE observations
                SET mention_count = ?, updated_at = ?
                WHERE id = ?
                ",
            params![mention_count.saturating_add(1), now, id],
        )?;
        drop(conn);
        return Ok(WriteOutcome::ExactDup);
    }

    let min_trust = trust_to_str(obs.min_trust);

    conn.execute(
        "
            INSERT INTO observations (
                agent_id,
                session_key,
                store,
                type,
                title,
                narrative,
                facts,
                tags,
                source,
                related_files,
                related_people,
                hash,
                mention_count,
                token_count,
                created_at,
                updated_at,
                expires_at,
                min_trust
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)
            ",
        params![
            memory.agent_id,
            obs.session_key,
            obs.store,
            obs.obs_type,
            obs.title,
            obs.narrative,
            facts_json,
            tags_json,
            obs.source,
            files_json,
            people_json,
            hash,
            token_count,
            now,
            now,
            expires_at,
            min_trust,
        ],
    )?;

    let id = conn.last_insert_rowid();

    conn.execute(
        "
            INSERT INTO observation_history (
                observation_id,
                old_title,
                old_facts,
                new_title,
                new_facts,
                event,
                created_at
            ) VALUES (?, NULL, NULL, ?, ?, 'ADD', ?)
            ",
        params![id, obs.title, facts_json, now],
    )?;

    for person in obs.related_people {
        conn.execute(
            "
                INSERT INTO people (
                    agent_id,
                    name,
                    store,
                    facts,
                    last_mentioned,
                    mention_count
                ) VALUES (?, ?, ?, '{}', ?, 1)
                ON CONFLICT(agent_id, name)
                DO UPDATE SET
                    store = excluded.store,
                    last_mentioned = excluded.last_mentioned,
                    mention_count = people.mention_count + 1
                ",
            params![memory.agent_id, person, obs.store, now],
        )?;
    }

    drop(conn);
    Ok(WriteOutcome::Added(id))
}

pub(super) fn people(memory: &SqliteMemory, query: &str) -> Result<Vec<Person>> {
    let needle = if query.trim().is_empty() {
        "%".to_owned()
    } else {
        format!("%{}%", query.trim())
    };

    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let mut stmt = conn.prepare(
        "
            SELECT name, store, facts, last_mentioned, mention_count
            FROM people
            WHERE agent_id = ?
              AND name LIKE ?
            ORDER BY mention_count DESC, COALESCE(last_mentioned, 0) DESC
            LIMIT 20
            ",
    )?;

    let rows = stmt.query_map(params![memory.agent_id, needle], |row| {
        let facts: String = row.get(2)?;
        let facts_value = serde_json::from_str(&facts).unwrap_or_else(|_| serde_json::json!({}));
        let last_mentioned: Option<i64> = row.get(3)?;
        let mention_count: u32 = row.get(4)?;
        Ok(Person {
            name: row.get(0)?,
            store: row.get(1)?,
            facts: facts_value,
            last_mentioned: last_mentioned.map(helpers::dt_from_ms),
            mention_count,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    drop(stmt);
    drop(conn);
    Ok(out)
}

pub(super) fn summarize_session(
    memory: &SqliteMemory,
    session_key: &SessionKey,
) -> Result<SessionSummary> {
    let session = session_key.to_string();
    let conn = memory.conn.lock().expect("memory db mutex poisoned");

    let mut stmt = conn.prepare(
        "
            SELECT title, type
            FROM observations
            WHERE agent_id = ?
              AND session_key = ?
            ORDER BY created_at ASC
            ",
    )?;

    let rows = stmt.query_map(params![memory.agent_id, session], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut titles = Vec::new();
    let mut decisions = Vec::new();
    let mut open_items = Vec::new();
    for row in rows {
        let (title, obs_type) = row?;
        if obs_type == "decision" {
            decisions.push(title.clone());
        }
        if obs_type == "task" {
            open_items.push(title.clone());
        }
        titles.push(title);
    }

    let now = chrono::Utc::now();
    let request = titles.first().cloned().unwrap_or_default();
    let outcome = titles.last().cloned().unwrap_or_default();
    let summary = SessionSummary {
        session_key: session_key.to_string(),
        request,
        outcome,
        decisions: decisions.clone(),
        open_items: open_items.clone(),
        observation_count: titles.len(),
        created_at: now,
    };

    conn.execute(
        "
            INSERT INTO session_summaries (
                agent_id,
                session_key,
                request,
                outcome,
                decisions,
                open_items,
                observation_count,
                created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ",
        params![
            memory.agent_id,
            summary.session_key,
            summary.request,
            summary.outcome,
            serde_json::to_string(&decisions)?,
            serde_json::to_string(&open_items)?,
            i64::try_from(summary.observation_count).unwrap_or(i64::MAX),
            helpers::ms_from_dt(summary.created_at),
        ],
    )?;

    drop(stmt);
    drop(conn);
    Ok(summary)
}

pub(super) fn history(
    memory: &SqliteMemory,
    observation_id: i64,
) -> Result<Vec<ObservationHistoryEntry>> {
    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let mut stmt = conn.prepare(
        "
            SELECT observation_id, old_title, old_facts, new_title, new_facts, event, created_at
            FROM observation_history
            WHERE observation_id = ?
            ORDER BY created_at ASC
            ",
    )?;

    let rows = stmt.query_map(params![observation_id], |row| {
        Ok(ObservationHistoryEntry {
            observation_id: row.get(0)?,
            old_title: row.get(1)?,
            old_facts: row.get(2)?,
            new_title: row.get(3)?,
            new_facts: row.get(4)?,
            event: row.get(5)?,
            created_at: helpers::dt_from_ms(row.get(6)?),
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    drop(stmt);
    drop(conn);
    Ok(out)
}
