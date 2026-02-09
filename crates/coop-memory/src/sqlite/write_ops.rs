use anyhow::Result;
use coop_core::{SessionKey, prompt::count_tokens};
use rusqlite::{OptionalExtension, params};
use tracing::{debug, info, warn};

use crate::types::{
    MemoryQuery, NewObservation, Observation, ObservationHistoryEntry, Person, ReconcileCandidate,
    ReconcileDecision, ReconcileObservation, ReconcileRequest, SessionSummary, WriteOutcome,
    min_trust_for_store, trust_to_str,
};

use super::{SqliteMemory, helpers, query};

const RECONCILE_LIMIT: usize = 6;
const RECONCILE_SCORE_THRESHOLD: f32 = 0.05;

#[derive(Debug, Clone)]
struct ObservationPayload {
    session_key: Option<String>,
    store: String,
    obs_type: String,
    title: String,
    narrative: String,
    facts: Vec<String>,
    tags: Vec<String>,
    source: String,
    related_files: Vec<String>,
    related_people: Vec<String>,
    token_count: u32,
    expires_at: Option<i64>,
    min_trust: String,
    hash: String,
}

impl ObservationPayload {
    fn from_new(obs: NewObservation) -> Self {
        let token_count = obs
            .token_count
            .unwrap_or_else(|| estimate_token_count(&obs.title, &obs.narrative, &obs.facts));
        let min_trust = trust_to_str(obs.min_trust).to_owned();
        let hash = helpers::observation_hash(&obs.title, &obs.facts);

        Self {
            session_key: obs.session_key,
            store: obs.store,
            obs_type: obs.obs_type,
            title: obs.title,
            narrative: obs.narrative,
            facts: obs.facts,
            tags: obs.tags,
            source: obs.source,
            related_files: obs.related_files,
            related_people: obs.related_people,
            token_count,
            expires_at: obs.expires_at.map(helpers::ms_from_dt),
            min_trust,
            hash,
        }
    }

    fn from_reconcile(base: &Self, merged: ReconcileObservation) -> Self {
        let store = if merged.store.trim().is_empty() {
            base.store.clone()
        } else {
            merged.store
        };
        let obs_type = if merged.obs_type.trim().is_empty() {
            base.obs_type.clone()
        } else {
            merged.obs_type
        };
        let title = if merged.title.trim().is_empty() {
            base.title.clone()
        } else {
            merged.title
        };

        let token_count = estimate_token_count(&title, &merged.narrative, &merged.facts);
        let hash = helpers::observation_hash(&title, &merged.facts);

        Self {
            session_key: base.session_key.clone(),
            store: store.clone(),
            obs_type,
            title,
            narrative: merged.narrative,
            facts: merged.facts,
            tags: merged.tags,
            source: base.source.clone(),
            related_files: merged.related_files,
            related_people: merged.related_people,
            token_count,
            expires_at: base.expires_at,
            min_trust: trust_to_str(min_trust_for_store(&store)).to_owned(),
            hash,
        }
    }

    fn to_reconcile_observation(&self) -> ReconcileObservation {
        ReconcileObservation {
            store: self.store.clone(),
            obs_type: self.obs_type.clone(),
            title: self.title.clone(),
            narrative: self.narrative.clone(),
            facts: self.facts.clone(),
            tags: self.tags.clone(),
            related_files: self.related_files.clone(),
            related_people: self.related_people.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CandidateMatch {
    id: i64,
    score: f32,
    observation: Observation,
}

pub(super) async fn write(memory: &SqliteMemory, obs: NewObservation) -> Result<WriteOutcome> {
    let now = helpers::now_ms();
    let incoming = ObservationPayload::from_new(obs);

    if bump_exact_duplicate(memory, &incoming.hash, now)? {
        info!("memory write exact duplicate");
        return Ok(WriteOutcome::ExactDup);
    }

    let candidates = find_reconciliation_candidates(memory, &incoming).await?;
    let decision = resolve_reconciliation(memory, &incoming, &candidates).await;

    apply_reconciliation_decision(memory, incoming, candidates, decision, now).await
}

fn bump_exact_duplicate(memory: &SqliteMemory, hash: &str, now: i64) -> Result<bool> {
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
        return Ok(true);
    }

    drop(conn);
    Ok(false)
}

async fn find_reconciliation_candidates(
    memory: &SqliteMemory,
    incoming: &ObservationPayload,
) -> Result<Vec<CandidateMatch>> {
    let reconcile_query = MemoryQuery {
        text: Some(incoming.title.clone()),
        stores: vec![incoming.store.clone()],
        types: vec![incoming.obs_type.clone()],
        people: incoming.related_people.clone(),
        limit: RECONCILE_LIMIT,
        max_tokens: None,
        ..Default::default()
    };

    let query_embedding = memory
        .embedding_for_observation(&incoming.title, &incoming.facts, "reconcile_candidates")
        .await;
    let ranked = query::search(memory, &reconcile_query, query_embedding.as_deref())?;

    let mut matches = Vec::new();
    for index in ranked {
        if index.score < RECONCILE_SCORE_THRESHOLD {
            break;
        }

        if let Some(observation) = memory.load_observation(index.id)? {
            matches.push(CandidateMatch {
                id: observation.id,
                score: index.score,
                observation,
            });
        }

        if matches.len() >= RECONCILE_LIMIT {
            break;
        }
    }

    debug!(
        candidate_count = matches.len(),
        "memory reconciliation candidates"
    );
    Ok(matches)
}

async fn resolve_reconciliation(
    memory: &SqliteMemory,
    incoming: &ObservationPayload,
    candidates: &[CandidateMatch],
) -> ReconcileDecision {
    if candidates.is_empty() {
        info!("memory reconciliation skipped: no similar candidates");
        return ReconcileDecision::Add;
    }

    let Some(reconciler) = memory.reconciler.as_ref() else {
        info!("memory reconciliation skipped: no reconciler configured");
        return ReconcileDecision::Add;
    };

    let request = ReconcileRequest {
        incoming: incoming.to_reconcile_observation(),
        candidates: candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| ReconcileCandidate {
                index,
                score: candidate.score,
                mention_count: candidate.observation.mention_count,
                created_at: candidate.observation.created_at,
                observation: ReconcileObservation {
                    store: candidate.observation.store.clone(),
                    obs_type: candidate.observation.obs_type.clone(),
                    title: candidate.observation.title.clone(),
                    narrative: candidate.observation.narrative.clone(),
                    facts: candidate.observation.facts.clone(),
                    tags: candidate.observation.tags.clone(),
                    related_files: candidate.observation.related_files.clone(),
                    related_people: candidate.observation.related_people.clone(),
                },
            })
            .collect(),
    };

    info!(
        candidate_count = request.candidates.len(),
        "memory reconciliation request"
    );

    let decision = match reconciler.reconcile(&request).await {
        Ok(decision) => decision,
        Err(error) => {
            warn!(error = %error, "memory reconciliation failed, defaulting to ADD");
            return ReconcileDecision::Add;
        }
    };

    if !decision_candidate_in_range(&decision, candidates.len()) {
        warn!(
            candidate_count = candidates.len(),
            ?decision,
            "memory reconciliation returned out-of-range candidate index"
        );
        return ReconcileDecision::Add;
    }

    info!(?decision, "memory reconciliation decision");
    decision
}

async fn apply_reconciliation_decision(
    memory: &SqliteMemory,
    incoming: ObservationPayload,
    candidates: Vec<CandidateMatch>,
    decision: ReconcileDecision,
    now: i64,
) -> Result<WriteOutcome> {
    match decision {
        ReconcileDecision::Add => {
            let id = insert_observation(memory, &incoming, now)?;
            embed_and_persist(memory, id, &incoming, now, "write_add").await;
            info!(observation_id = id, "memory reconciliation applied: ADD");
            Ok(WriteOutcome::Added(id))
        }
        ReconcileDecision::Update {
            candidate_index,
            merged,
        } => {
            let Some(candidate) = candidates.get(candidate_index) else {
                return Ok(WriteOutcome::Skipped);
            };
            let merged_payload = ObservationPayload::from_reconcile(&incoming, merged);
            apply_update(memory, candidate, &merged_payload, now).await
        }
        ReconcileDecision::Delete { candidate_index } => {
            let Some(candidate) = candidates.get(candidate_index) else {
                return Ok(WriteOutcome::Skipped);
            };
            apply_delete(memory, candidate, &incoming, now).await
        }
        ReconcileDecision::None { candidate_index } => {
            let Some(candidate) = candidates.get(candidate_index) else {
                return Ok(WriteOutcome::Skipped);
            };
            bump_candidate_mention(memory, candidate.id, now)?;
            info!(
                observation_id = candidate.id,
                "memory reconciliation applied: NONE"
            );
            Ok(WriteOutcome::Skipped)
        }
    }
}

async fn apply_update(
    memory: &SqliteMemory,
    candidate: &CandidateMatch,
    merged: &ObservationPayload,
    now: i64,
) -> Result<WriteOutcome> {
    let old_facts = helpers::to_json(&candidate.observation.facts);
    let new_facts = helpers::to_json(&merged.facts);
    let tags_json = helpers::to_json(&merged.tags);
    let files_json = helpers::to_json(&merged.related_files);
    let people_json = helpers::to_json(&merged.related_people);

    {
        let conn = memory.conn.lock().expect("memory db mutex poisoned");
        conn.execute(
            "
                UPDATE observations
                SET session_key = ?,
                    store = ?,
                    type = ?,
                    title = ?,
                    narrative = ?,
                    facts = ?,
                    tags = ?,
                    source = ?,
                    related_files = ?,
                    related_people = ?,
                    hash = ?,
                    mention_count = mention_count + 1,
                    token_count = ?,
                    updated_at = ?,
                    expires_at = ?,
                    min_trust = ?
                WHERE id = ?
                  AND agent_id = ?
                ",
            params![
                merged.session_key,
                merged.store,
                merged.obs_type,
                merged.title,
                merged.narrative,
                new_facts,
                tags_json,
                merged.source,
                files_json,
                people_json,
                merged.hash,
                merged.token_count,
                now,
                merged.expires_at,
                merged.min_trust,
                candidate.id,
                memory.agent_id,
            ],
        )?;

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
                ) VALUES (?, ?, ?, ?, ?, 'UPDATE', ?)
                ",
            params![
                candidate.id,
                candidate.observation.title,
                old_facts,
                merged.title,
                helpers::to_json(&merged.facts),
                now,
            ],
        )?;

        upsert_people(
            &conn,
            &memory.agent_id,
            &merged.store,
            &merged.related_people,
            now,
        )?;
        drop(conn);
    }

    embed_and_persist(memory, candidate.id, merged, now, "write_update").await;
    info!(
        observation_id = candidate.id,
        "memory reconciliation applied: UPDATE"
    );
    Ok(WriteOutcome::Updated(candidate.id))
}

async fn apply_delete(
    memory: &SqliteMemory,
    candidate: &CandidateMatch,
    replacement: &ObservationPayload,
    now: i64,
) -> Result<WriteOutcome> {
    let old_facts = helpers::to_json(&candidate.observation.facts);

    {
        let conn = memory.conn.lock().expect("memory db mutex poisoned");
        conn.execute(
            "
                UPDATE observations
                SET expires_at = ?,
                    updated_at = ?
                WHERE id = ?
                  AND agent_id = ?
                ",
            params![now, now, candidate.id, memory.agent_id],
        )?;

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
                ) VALUES (?, ?, ?, ?, ?, 'DELETE', ?)
                ",
            params![
                candidate.id,
                candidate.observation.title,
                old_facts,
                replacement.title,
                helpers::to_json(&replacement.facts),
                now,
            ],
        )?;
        drop(conn);
    }

    if let Err(error) = memory.remove_embedding(candidate.id) {
        warn!(
            observation_id = candidate.id,
            error = %error,
            "failed to remove stale observation embedding"
        );
    }

    let new_id = insert_observation(memory, replacement, now)?;
    embed_and_persist(memory, new_id, replacement, now, "write_add_after_delete").await;

    info!(
        deleted_observation_id = candidate.id,
        replacement_observation_id = new_id,
        "memory reconciliation applied: DELETE"
    );

    Ok(WriteOutcome::Deleted(candidate.id))
}

fn insert_observation(memory: &SqliteMemory, obs: &ObservationPayload, now: i64) -> Result<i64> {
    let facts_json = helpers::to_json(&obs.facts);
    let tags_json = helpers::to_json(&obs.tags);
    let files_json = helpers::to_json(&obs.related_files);
    let people_json = helpers::to_json(&obs.related_people);

    let conn = memory.conn.lock().expect("memory db mutex poisoned");

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
            obs.hash,
            obs.token_count,
            now,
            now,
            obs.expires_at,
            obs.min_trust,
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
        params![id, obs.title, helpers::to_json(&obs.facts), now],
    )?;

    upsert_people(
        &conn,
        &memory.agent_id,
        &obs.store,
        &obs.related_people,
        now,
    )?;

    drop(conn);
    Ok(id)
}

fn upsert_people(
    conn: &rusqlite::Connection,
    agent_id: &str,
    store: &str,
    related_people: &[String],
    now: i64,
) -> Result<()> {
    for person in related_people {
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
            params![agent_id, person, store, now],
        )?;
    }

    Ok(())
}

fn bump_candidate_mention(memory: &SqliteMemory, observation_id: i64, now: i64) -> Result<()> {
    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    conn.execute(
        "
            UPDATE observations
            SET mention_count = mention_count + 1,
                updated_at = ?
            WHERE id = ?
              AND agent_id = ?
            ",
        params![now, observation_id, memory.agent_id],
    )?;
    drop(conn);
    Ok(())
}

async fn embed_and_persist(
    memory: &SqliteMemory,
    observation_id: i64,
    obs: &ObservationPayload,
    now: i64,
    reason: &'static str,
) {
    let Some(embedding) = memory
        .embedding_for_observation(&obs.title, &obs.facts, reason)
        .await
    else {
        return;
    };

    if let Err(error) = memory.persist_embedding(observation_id, &embedding, now) {
        warn!(
            observation_id,
            error = %error,
            "failed to persist observation embedding"
        );
    }
}

fn estimate_token_count(title: &str, narrative: &str, facts: &[String]) -> u32 {
    let mut text = title.to_owned();
    if !narrative.is_empty() {
        text.push(' ');
        text.push_str(narrative);
    }
    if !facts.is_empty() {
        text.push(' ');
        text.push_str(&facts.join("; "));
    }
    u32::try_from(count_tokens(&text)).unwrap_or(u32::MAX)
}

fn decision_candidate_in_range(decision: &ReconcileDecision, candidate_count: usize) -> bool {
    match decision {
        ReconcileDecision::Add => true,
        ReconcileDecision::Update {
            candidate_index, ..
        }
        | ReconcileDecision::Delete { candidate_index }
        | ReconcileDecision::None { candidate_index } => *candidate_index < candidate_count,
    }
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
