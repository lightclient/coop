use anyhow::Result;
use coop_core::prompt::count_tokens;
use rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use tracing::info;

use crate::types::{MemoryMaintenanceConfig, MemoryMaintenanceReport};

use super::{SqliteMemory, helpers};

const DAY_MS: i64 = 86_400_000;

#[derive(Debug)]
struct CompressionCandidate {
    id: i64,
    session_key: Option<String>,
    store: String,
    obs_type: String,
    title: String,
    facts: Vec<String>,
    tags: Vec<String>,
    related_files: Vec<String>,
    related_people: Vec<String>,
    mention_count: u32,
    min_trust: String,
}

#[derive(Debug)]
struct ArchiveCandidate {
    id: i64,
    session_key: Option<String>,
    store: String,
    obs_type: String,
    title: String,
    narrative: String,
    facts_json: String,
    tags_json: String,
    source: String,
    related_files_json: String,
    related_people_json: String,
    hash: String,
    mention_count: u32,
    token_count: Option<i64>,
    created_at: i64,
    updated_at: i64,
    expires_at: Option<i64>,
    min_trust: String,
}

#[derive(Debug, Default)]
struct CompressionStats {
    scanned: usize,
    compressed: usize,
    summaries: usize,
}

#[derive(Debug, Default)]
struct ArchiveStats {
    scanned: usize,
    archived: usize,
}

#[derive(Debug, Default)]
struct CleanupStats {
    scanned: usize,
    deleted: usize,
}

pub(super) fn run(
    memory: &SqliteMemory,
    config: &MemoryMaintenanceConfig,
) -> Result<MemoryMaintenanceReport> {
    let started = Instant::now();
    info!(
        archive_after_days = config.archive_after_days,
        delete_archive_after_days = config.delete_archive_after_days,
        compress_after_days = config.compress_after_days,
        compression_min_cluster_size = config.compression_min_cluster_size,
        max_rows_per_run = config.max_rows_per_run,
        "memory maintenance run started"
    );

    let now_ms = helpers::now_ms();

    let compression = compress_stale_observations(memory, config, now_ms)?;
    info!(
        scanned_rows = compression.scanned,
        compressed_rows = compression.compressed,
        summary_rows = compression.summaries,
        "memory maintenance compression stage complete"
    );

    let archive = archive_observations(memory, config, now_ms)?;
    info!(
        scanned_rows = archive.scanned,
        archived_rows = archive.archived,
        "memory maintenance archive stage complete"
    );

    let cleanup = cleanup_archive(memory, config, now_ms)?;
    info!(
        scanned_rows = cleanup.scanned,
        deleted_rows = cleanup.deleted,
        "memory maintenance archive cleanup stage complete"
    );

    let report = MemoryMaintenanceReport {
        compressed_rows: compression.compressed,
        summary_rows: compression.summaries,
        archived_rows: archive.archived,
        archive_deleted_rows: cleanup.deleted,
    };

    info!(
        compressed_rows = report.compressed_rows,
        summary_rows = report.summary_rows,
        archived_rows = report.archived_rows,
        archive_deleted_rows = report.archive_deleted_rows,
        duration_ms = started.elapsed().as_millis(),
        "memory maintenance run complete"
    );

    Ok(report)
}

#[allow(clippy::too_many_lines)]
fn compress_stale_observations(
    memory: &SqliteMemory,
    config: &MemoryMaintenanceConfig,
    now_ms: i64,
) -> Result<CompressionStats> {
    let stale_cutoff = now_ms - config.compress_after_days.saturating_mul(DAY_MS);
    let fetch_limit = config
        .max_rows_per_run
        .saturating_mul(config.compression_min_cluster_size.max(1));

    let mut conn = memory.conn.lock().expect("memory db mutex poisoned");

    let candidates = {
        let mut stmt = conn.prepare(
            "
            SELECT
                id,
                session_key,
                store,
                type,
                title,
                facts,
                tags,
                related_files,
                related_people,
                mention_count,
                min_trust
            FROM observations
            WHERE agent_id = ?
              AND created_at <= ?
              AND (expires_at IS NULL OR expires_at > ?)
            ORDER BY store ASC, type ASC, lower(title) ASC, created_at ASC
            LIMIT ?
            ",
        )?;

        let rows = stmt.query_map(
            params![
                memory.agent_id,
                stale_cutoff,
                now_ms,
                i64::try_from(fetch_limit).unwrap_or(i64::MAX)
            ],
            |row| {
                let facts_json: String = row.get(5)?;
                let tags_json: String = row.get(6)?;
                let related_files_json: String = row.get(7)?;
                let related_people_json: String = row.get(8)?;

                Ok(CompressionCandidate {
                    id: row.get(0)?,
                    session_key: row.get(1)?,
                    store: row.get(2)?,
                    obs_type: row.get(3)?,
                    title: row.get(4)?,
                    facts: helpers::from_json(&facts_json),
                    tags: helpers::from_json(&tags_json),
                    related_files: helpers::from_json(&related_files_json),
                    related_people: helpers::from_json(&related_people_json),
                    mention_count: row.get::<_, Option<u32>>(9)?.unwrap_or(0),
                    min_trust: row.get(10)?,
                })
            },
        )?;

        let mut loaded = Vec::new();
        for row in rows {
            loaded.push(row?);
        }
        loaded
    };

    if candidates.is_empty() {
        return Ok(CompressionStats::default());
    }

    let mut clusters: BTreeMap<(String, String, String), Vec<CompressionCandidate>> =
        BTreeMap::new();
    for candidate in candidates {
        let key = (
            candidate.store.clone(),
            candidate.obs_type.clone(),
            normalize_title(&candidate.title),
        );
        clusters.entry(key).or_default().push(candidate);
    }

    let mut stats = CompressionStats {
        scanned: clusters.values().map(Vec::len).sum(),
        ..CompressionStats::default()
    };

    for cluster in clusters.values() {
        if cluster.len() < config.compression_min_cluster_size {
            continue;
        }

        if stats.compressed.saturating_add(cluster.len()) > config.max_rows_per_run {
            continue;
        }

        let summary_title = format!("{} (compressed {})", cluster[0].title, cluster.len());
        let summary_narrative = format!(
            "Deterministic summary from {} observations in the '{}' / '{}' cluster.",
            cluster.len(),
            cluster[0].store,
            cluster[0].obs_type,
        );

        let summary_facts = union_sorted(cluster.iter().flat_map(|row| row.facts.iter().cloned()));
        let summary_tags = {
            let mut tags = union_sorted(cluster.iter().flat_map(|row| row.tags.iter().cloned()));
            tags.push("compressed".to_owned());
            tags.sort();
            tags.dedup();
            tags
        };
        let summary_files = union_sorted(
            cluster
                .iter()
                .flat_map(|row| row.related_files.iter().cloned()),
        );
        let summary_people = union_sorted(
            cluster
                .iter()
                .flat_map(|row| row.related_people.iter().cloned()),
        );

        let mention_count = cluster
            .iter()
            .fold(0_u32, |acc, row| acc.saturating_add(row.mention_count))
            .max(1);

        let token_count = estimate_token_count(&summary_title, &summary_narrative, &summary_facts);
        let hash = helpers::observation_hash(&summary_title, &summary_facts);
        let summary_facts_json = helpers::to_json(&summary_facts);

        let tx = conn.transaction()?;

        tx.execute(
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
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)
            ",
            params![
                memory.agent_id,
                cluster[0].session_key.clone(),
                cluster[0].store.clone(),
                cluster[0].obs_type.clone(),
                summary_title.clone(),
                summary_narrative.clone(),
                summary_facts_json.clone(),
                helpers::to_json(&summary_tags),
                "maintenance",
                helpers::to_json(&summary_files),
                helpers::to_json(&summary_people),
                hash,
                mention_count,
                i64::from(token_count),
                now_ms,
                now_ms,
                cluster[0].min_trust.clone(),
            ],
        )?;

        let summary_id = tx.last_insert_rowid();

        tx.execute(
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
            params![
                summary_id,
                summary_title.clone(),
                helpers::to_json(&summary_facts),
                now_ms
            ],
        )?;

        for row in cluster {
            tx.execute(
                "
                UPDATE observations
                SET expires_at = ?, updated_at = ?
                WHERE id = ?
                  AND agent_id = ?
                ",
                params![now_ms, now_ms, row.id, memory.agent_id],
            )?;

            tx.execute(
                "
                INSERT INTO observation_history (
                    observation_id,
                    old_title,
                    old_facts,
                    new_title,
                    new_facts,
                    event,
                    created_at
                ) VALUES (?, ?, ?, ?, ?, 'COMPRESS', ?)
                ",
                params![
                    row.id,
                    row.title,
                    helpers::to_json(&row.facts),
                    summary_title.clone(),
                    helpers::to_json(&summary_facts),
                    now_ms,
                ],
            )?;
        }

        tx.commit()?;

        stats.compressed = stats.compressed.saturating_add(cluster.len());
        stats.summaries = stats.summaries.saturating_add(1);
    }

    drop(conn);
    Ok(stats)
}

#[allow(clippy::too_many_lines)]
fn archive_observations(
    memory: &SqliteMemory,
    config: &MemoryMaintenanceConfig,
    now_ms: i64,
) -> Result<ArchiveStats> {
    let archive_cutoff = now_ms - config.archive_after_days.saturating_mul(DAY_MS);
    let mut conn = memory.conn.lock().expect("memory db mutex poisoned");

    let candidates = {
        let mut stmt = conn.prepare(
            "
            SELECT
                id,
                session_key,
                store,
                type,
                title,
                COALESCE(narrative, ''),
                facts,
                tags,
                COALESCE(source, ''),
                related_files,
                related_people,
                hash,
                mention_count,
                token_count,
                created_at,
                updated_at,
                expires_at,
                min_trust
            FROM observations
            WHERE agent_id = ?
              AND (created_at <= ? OR (expires_at IS NOT NULL AND expires_at <= ?))
            ORDER BY COALESCE(expires_at, created_at) ASC
            LIMIT ?
            ",
        )?;

        let rows = stmt.query_map(
            params![
                memory.agent_id,
                archive_cutoff,
                archive_cutoff,
                i64::try_from(config.max_rows_per_run).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok(ArchiveCandidate {
                    id: row.get(0)?,
                    session_key: row.get(1)?,
                    store: row.get(2)?,
                    obs_type: row.get(3)?,
                    title: row.get(4)?,
                    narrative: row.get(5)?,
                    facts_json: row.get(6)?,
                    tags_json: row.get(7)?,
                    source: row.get(8)?,
                    related_files_json: row.get(9)?,
                    related_people_json: row.get(10)?,
                    hash: row.get(11)?,
                    mention_count: row.get::<_, Option<u32>>(12)?.unwrap_or(0),
                    token_count: row.get(13)?,
                    created_at: row.get(14)?,
                    updated_at: row.get(15)?,
                    expires_at: row.get(16)?,
                    min_trust: row.get(17)?,
                })
            },
        )?;

        let mut loaded = Vec::new();
        for row in rows {
            loaded.push(row?);
        }
        loaded
    };

    if candidates.is_empty() {
        return Ok(ArchiveStats::default());
    }

    let tx = conn.transaction()?;
    let mut archived = 0_usize;

    for row in &candidates {
        let archive_reason = if row
            .expires_at
            .is_some_and(|expires| expires <= archive_cutoff)
        {
            "expired"
        } else {
            "age"
        };

        tx.execute(
            "
            INSERT INTO observation_archive (
                original_observation_id,
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
                min_trust,
                archived_at,
                archive_reason
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ",
            params![
                row.id,
                memory.agent_id,
                row.session_key,
                row.store,
                row.obs_type,
                row.title,
                row.narrative,
                row.facts_json,
                row.tags_json,
                row.source,
                row.related_files_json,
                row.related_people_json,
                row.hash,
                row.mention_count,
                row.token_count,
                row.created_at,
                row.updated_at,
                row.expires_at,
                row.min_trust,
                now_ms,
                archive_reason,
            ],
        )?;

        tx.execute(
            "DELETE FROM observations WHERE id = ? AND agent_id = ?",
            params![row.id, memory.agent_id],
        )?;

        archived = archived.saturating_add(1);
    }

    tx.commit()?;

    drop(conn);
    Ok(ArchiveStats {
        scanned: candidates.len(),
        archived,
    })
}

fn cleanup_archive(
    memory: &SqliteMemory,
    config: &MemoryMaintenanceConfig,
    now_ms: i64,
) -> Result<CleanupStats> {
    let cleanup_cutoff = now_ms - config.delete_archive_after_days.saturating_mul(DAY_MS);

    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let mut stmt = conn.prepare(
        "
        SELECT id
        FROM observation_archive
        WHERE agent_id = ?
          AND archived_at <= ?
        ORDER BY archived_at ASC
        LIMIT ?
        ",
    )?;

    let ids = stmt
        .query_map(
            params![
                memory.agent_id,
                cleanup_cutoff,
                i64::try_from(config.max_rows_per_run).unwrap_or(i64::MAX)
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut deleted = 0_usize;
    for id in &ids {
        deleted = deleted.saturating_add(conn.execute(
            "DELETE FROM observation_archive WHERE id = ? AND agent_id = ?",
            params![id, memory.agent_id],
        )?);
    }

    drop(conn);
    Ok(CleanupStats {
        scanned: ids.len(),
        deleted,
    })
}

fn union_sorted(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut set = BTreeSet::new();
    for value in items {
        let normalized = value.trim();
        if !normalized.is_empty() {
            set.insert(normalized.to_owned());
        }
    }
    set.into_iter().collect()
}

fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
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
