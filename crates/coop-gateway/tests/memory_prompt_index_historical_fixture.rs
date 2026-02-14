#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use coop_core::{SessionKey, SessionKind, TrustLevel};
use coop_memory::{
    Memory, MemoryMaintenanceConfig, MemoryMaintenanceReport, MemoryQuery, NewObservation,
    Observation, ObservationHistoryEntry, ObservationIndex, Person, SessionSummary, WriteOutcome,
};
use serde::Deserialize;

#[path = "../src/config.rs"]
mod config;
#[path = "../src/memory_prompt_index.rs"]
mod memory_prompt_index;

use config::MemoryPromptIndexConfig;

const HISTORICAL_FIXTURE: &str = include_str!("fixtures/historical_observations.json");

#[derive(Debug, Deserialize)]
struct FixtureObservation {
    id: i64,
    title: String,
    obs_type: String,
    store: String,
    days_ago: i64,
    mention_count: u32,
    score: f32,
    related_people: Vec<String>,
}

#[derive(Debug, Clone)]
struct HistoricalMemory {
    now: chrono::DateTime<Utc>,
    rows: Vec<ObservationIndex>,
}

impl HistoricalMemory {
    fn load() -> Self {
        let now = Utc::now();
        let fixture_rows: Vec<FixtureObservation> =
            serde_json::from_str(HISTORICAL_FIXTURE).unwrap();

        let rows = fixture_rows
            .into_iter()
            .map(|row| ObservationIndex {
                id: row.id,
                title: row.title,
                obs_type: row.obs_type,
                store: row.store,
                created_at: now - Duration::days(row.days_ago),
                token_count: 48,
                mention_count: row.mention_count,
                score: row.score,
                related_people: row.related_people,
            })
            .collect();

        Self { now, rows }
    }

    fn only_older_than_days(&self, min_days: i64) -> Self {
        let cutoff = self.now - Duration::days(min_days);
        let rows = self
            .rows
            .iter()
            .filter(|row| row.created_at < cutoff)
            .cloned()
            .collect();
        Self {
            now: self.now,
            rows,
        }
    }

    fn recent_ids(&self, recent_days: u32) -> Vec<i64> {
        let cutoff = self.now - Duration::days(i64::from(recent_days));
        let mut rows = self
            .rows
            .iter()
            .filter(|row| row.created_at >= cutoff)
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        rows.into_iter().map(|row| row.id).collect()
    }
}

#[async_trait]
impl Memory for HistoricalMemory {
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ObservationIndex>> {
        let mut rows = self.rows.clone();

        if !query.stores.is_empty() {
            rows.retain(|row| query.stores.contains(&row.store));
        }

        if let Some(after) = query.after {
            rows.retain(|row| row.created_at >= after);
        }

        if let Some(text) = &query.text {
            if !text.trim().is_empty() {
                rows.sort_by(|a, b| {
                    b.mention_count
                        .cmp(&a.mention_count)
                        .then_with(|| b.created_at.cmp(&a.created_at))
                });
            }
        } else {
            rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        }

        rows.truncate(query.limit);
        Ok(rows)
    }

    async fn timeline(
        &self,
        _anchor: i64,
        _before: usize,
        _after: usize,
    ) -> Result<Vec<ObservationIndex>> {
        Ok(Vec::new())
    }

    async fn get(&self, _ids: &[i64]) -> Result<Vec<Observation>> {
        Ok(Vec::new())
    }

    async fn write(&self, _obs: NewObservation) -> Result<WriteOutcome> {
        Ok(WriteOutcome::Skipped)
    }

    async fn people(&self, _query: &str) -> Result<Vec<Person>> {
        Ok(Vec::new())
    }

    async fn summarize_session(&self, session_key: &SessionKey) -> Result<SessionSummary> {
        Ok(SessionSummary {
            session_key: session_key.to_string(),
            request: String::new(),
            outcome: String::new(),
            decisions: Vec::new(),
            open_items: Vec::new(),
            observation_count: 0,
            created_at: Utc::now(),
        })
    }

    async fn recent_session_summaries(&self, _limit: usize) -> Result<Vec<SessionSummary>> {
        Ok(vec![SessionSummary {
            session_key: SessionKey {
                agent_id: "coop".to_owned(),
                kind: SessionKind::Main,
            }
            .to_string(),
            request: "historical fixture bootstrap".to_owned(),
            outcome: "loaded fixture rows".to_owned(),
            decisions: Vec::new(),
            open_items: Vec::new(),
            observation_count: self.rows.len(),
            created_at: Utc::now(),
        }])
    }

    async fn history(&self, _observation_id: i64) -> Result<Vec<ObservationHistoryEntry>> {
        Ok(Vec::new())
    }

    async fn run_maintenance(
        &self,
        _config: &MemoryMaintenanceConfig,
    ) -> Result<MemoryMaintenanceReport> {
        Ok(MemoryMaintenanceReport::default())
    }
}

fn parse_prompt_ids(block: &str) -> Vec<i64> {
    block
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("- id=")?;
            let id = rest.split_whitespace().next()?;
            id.parse::<i64>().ok()
        })
        .collect()
}

#[tokio::test]
async fn fixture_recency_window_is_always_first_under_load() {
    let memory = HistoricalMemory::load();
    let settings = MemoryPromptIndexConfig {
        enabled: true,
        include_file_links: true,
        limit: 8,
        max_tokens: 3_000,
        recent_days: 3,
    };

    let block = memory_prompt_index::build_prompt_index(
        &memory,
        TrustLevel::Full,
        &settings,
        "migration incident summary",
    )
    .await
    .unwrap()
    .unwrap();

    let rendered_ids = parse_prompt_ids(&block);
    let recent_ids = memory.recent_ids(settings.recent_days);

    assert!(!recent_ids.is_empty(), "fixture must include recent rows");
    assert!(
        rendered_ids.len() >= recent_ids.len(),
        "expected recent rows to render first"
    );

    let rendered_recent_prefix = &rendered_ids[..recent_ids.len()];
    assert_eq!(
        rendered_recent_prefix,
        recent_ids.as_slice(),
        "recent rows should be the first rows in the prompt index"
    );

    assert!(
        rendered_ids.iter().any(|id| [110, 111, 114].contains(id)),
        "expected older high-relevance rows to fill remaining slots"
    );
}

#[tokio::test]
async fn fixture_with_no_recent_rows_allocates_full_limit_to_relevance() {
    let older_only = HistoricalMemory::load().only_older_than_days(5);
    let settings = MemoryPromptIndexConfig {
        enabled: true,
        include_file_links: true,
        limit: 6,
        max_tokens: 3_000,
        recent_days: 3,
    };

    let block = memory_prompt_index::build_prompt_index(
        &older_only,
        TrustLevel::Full,
        &settings,
        "migration incident",
    )
    .await
    .unwrap()
    .unwrap();

    let rendered_ids = parse_prompt_ids(&block);

    assert_eq!(rendered_ids.len(), 6);
    assert_eq!(rendered_ids[0], 114);
    assert_eq!(rendered_ids[1], 110);
    assert_eq!(rendered_ids[2], 111);
}
