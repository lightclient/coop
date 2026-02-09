#![allow(clippy::unwrap_used)]

use anyhow::Result;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::traits::{EmbeddingProvider, Memory, Reconciler};
use crate::types::{
    MemoryMaintenanceConfig, MemoryQuery, NewObservation, ReconcileDecision, ReconcileObservation,
    ReconcileRequest, WriteOutcome, min_trust_for_store, trust_from_str, trust_to_str,
};
use coop_core::{SessionKey, SessionKind, TrustLevel};

use super::SqliteMemory;

#[derive(Debug)]
struct RecordingEmbedder {
    dimensions: usize,
    calls: Mutex<Vec<String>>,
}

impl RecordingEmbedder {
    fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl EmbeddingProvider for RecordingEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.calls.lock().unwrap().push(text.to_owned());

        let mut out = vec![0.0; self.dimensions];
        for (idx, byte) in text.bytes().take(self.dimensions).enumerate() {
            out[idx] = f32::from(byte) / 255.0;
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[derive(Debug)]
struct QueueReconciler {
    decisions: Mutex<VecDeque<ReconcileDecision>>,
    requests: Mutex<Vec<ReconcileRequest>>,
}

impl QueueReconciler {
    fn new(decisions: Vec<ReconcileDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ReconcileRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Reconciler for QueueReconciler {
    async fn reconcile(&self, request: &ReconcileRequest) -> Result<ReconcileDecision> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ReconcileDecision::Add))
    }
}

fn memory() -> SqliteMemory {
    memory_with(None, None)
}

fn memory_with(
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    reconciler: Option<Arc<dyn Reconciler>>,
) -> SqliteMemory {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let memory = SqliteMemory::open_with_components(path, "coop", embedder, reconciler).unwrap();
    std::mem::forget(dir);
    memory
}

fn sample_obs(title: &str, facts: &[&str]) -> NewObservation {
    NewObservation {
        session_key: Some("coop:main".to_owned()),
        store: "shared".to_owned(),
        obs_type: "technical".to_owned(),
        title: title.to_owned(),
        narrative: format!("narrative for {title}"),
        facts: facts.iter().map(|f| (*f).to_owned()).collect(),
        tags: vec!["test".to_owned()],
        source: "agent".to_owned(),
        related_files: vec!["src/main.rs".to_owned()],
        related_people: vec!["alice".to_owned()],
        token_count: Some(50),
        expires_at: None,
        min_trust: min_trust_for_store("shared"),
    }
}

fn merged_obs(title: &str, facts: &[&str]) -> ReconcileObservation {
    ReconcileObservation {
        store: "shared".to_owned(),
        obs_type: "technical".to_owned(),
        title: title.to_owned(),
        narrative: format!("merged narrative for {title}"),
        facts: facts.iter().map(|f| (*f).to_owned()).collect(),
        tags: vec!["merged".to_owned()],
        related_files: vec!["src/lib.rs".to_owned()],
        related_people: vec!["alice".to_owned()],
    }
}

fn embedding_row_count(memory: &SqliteMemory) -> i64 {
    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let count = conn
        .query_row("SELECT COUNT(*) FROM observation_embeddings", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    drop(conn);
    count
}

fn archive_row_count(memory: &SqliteMemory) -> i64 {
    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let count = conn
        .query_row("SELECT COUNT(*) FROM observation_archive", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    drop(conn);
    count
}

fn set_observation_created_at(memory: &SqliteMemory, id: i64, created_at: i64) {
    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    conn.execute(
        "UPDATE observations SET created_at = ?, updated_at = ? WHERE id = ?",
        rusqlite::params![created_at, created_at, id],
    )
    .unwrap();
    drop(conn);
}

fn set_archive_archived_at(memory: &SqliteMemory, archived_at: i64) {
    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    conn.execute(
        "UPDATE observation_archive SET archived_at = ?",
        rusqlite::params![archived_at],
    )
    .unwrap();
    drop(conn);
}

#[tokio::test]
async fn write_and_get_round_trip() {
    let memory = memory();
    let outcome = memory
        .write(sample_obs("first", &["fact one"]))
        .await
        .unwrap();
    let id = match outcome {
        WriteOutcome::Added(id) => id,
        other => panic!("unexpected outcome: {other:?}"),
    };

    let obs = memory.get(&[id]).await.unwrap();
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].title, "first");
    assert_eq!(obs[0].store, "shared");
}

#[tokio::test]
async fn exact_dup_bumps_mention_count() {
    let memory = memory();
    let first = memory.write(sample_obs("dup", &["fact"])).await.unwrap();
    assert!(matches!(first, WriteOutcome::Added(_)));

    let second = memory.write(sample_obs("dup", &["fact"])).await.unwrap();
    assert_eq!(second, WriteOutcome::ExactDup);

    let found = memory
        .search(&MemoryQuery {
            text: Some("dup".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].mention_count, 2);
}

#[tokio::test]
async fn embeddings_persist_for_add_update_not_exactdup_or_none() {
    let embedder = Arc::new(RecordingEmbedder::new(6));
    let reconciler = Arc::new(QueueReconciler::new(vec![
        ReconcileDecision::Update {
            candidate_index: 0,
            merged: merged_obs("service status", &["healthy", "stable"]),
        },
        ReconcileDecision::None { candidate_index: 0 },
    ]));

    let memory = memory_with(
        Some(Arc::clone(&embedder) as Arc<dyn EmbeddingProvider>),
        Some(Arc::clone(&reconciler) as Arc<dyn Reconciler>),
    );

    let first = memory
        .write(sample_obs("service status", &["healthy"]))
        .await
        .unwrap();
    let WriteOutcome::Added(id) = first else {
        panic!("expected add, got {first:?}");
    };
    assert_eq!(embedding_row_count(&memory), 1);

    let updated = memory
        .write(sample_obs("service status", &["degraded"]))
        .await
        .unwrap();
    assert_eq!(updated, WriteOutcome::Updated(id));
    assert_eq!(embedding_row_count(&memory), 1);

    let exact = memory
        .write(sample_obs("service status", &["healthy", "stable"]))
        .await
        .unwrap();
    assert_eq!(exact, WriteOutcome::ExactDup);
    assert_eq!(embedding_row_count(&memory), 1);

    let none = memory
        .write(sample_obs("service status", &["same mention"]))
        .await
        .unwrap();
    assert_eq!(none, WriteOutcome::Skipped);
    assert_eq!(embedding_row_count(&memory), 1);

    assert!(embedder.call_count() >= 3);
}

#[tokio::test]
async fn reconciliation_update_records_history_and_dense_indices() {
    let reconciler = Arc::new(QueueReconciler::new(vec![ReconcileDecision::Update {
        candidate_index: 0,
        merged: merged_obs("deploy checklist", &["build", "test", "release"]),
    }]));
    let memory = memory_with(None, Some(Arc::clone(&reconciler) as Arc<dyn Reconciler>));

    let first = memory
        .write(sample_obs("deploy checklist", &["build", "test"]))
        .await
        .unwrap();
    let WriteOutcome::Added(id) = first else {
        panic!("expected add, got {first:?}");
    };

    let second = memory
        .write(sample_obs("deploy checklist", &["release"]))
        .await
        .unwrap();
    assert_eq!(second, WriteOutcome::Updated(id));

    let obs = memory.get(&[id]).await.unwrap();
    assert_eq!(obs[0].facts, vec!["build", "test", "release"]);

    let history = memory.history(id).await.unwrap();
    let events = history
        .iter()
        .map(|entry| entry.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(events, vec!["ADD", "UPDATE"]);
    let update = history.last().unwrap();
    assert!(update.old_title.is_some());
    assert!(update.old_facts.is_some());
    assert!(update.new_title.is_some());
    assert!(update.new_facts.is_some());

    let requests = reconciler.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .candidates
            .iter()
            .enumerate()
            .all(|(idx, candidate)| candidate.index == idx)
    );
}

#[tokio::test]
async fn reconciliation_delete_expires_stale_and_records_history() {
    let reconciler = Arc::new(QueueReconciler::new(vec![ReconcileDecision::Delete {
        candidate_index: 0,
    }]));
    let memory = memory_with(None, Some(Arc::clone(&reconciler) as Arc<dyn Reconciler>));

    let first = memory
        .write(sample_obs("rotation key", &["v1"]))
        .await
        .unwrap();
    let WriteOutcome::Added(old_id) = first else {
        panic!("expected add, got {first:?}");
    };

    let result = memory
        .write(sample_obs("rotation key", &["v2"]))
        .await
        .unwrap();
    assert_eq!(result, WriteOutcome::Deleted(old_id));

    let old = memory.get(&[old_id]).await.unwrap();
    assert!(old.is_empty());

    let search = memory
        .search(&MemoryQuery {
            text: Some("rotation key".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(search.len(), 1);
    assert_ne!(search[0].id, old_id);

    let history = memory.history(old_id).await.unwrap();
    let events = history
        .iter()
        .map(|entry| entry.event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(events, vec!["ADD", "DELETE"]);
    let delete = history.last().unwrap();
    assert!(delete.old_title.is_some());
    assert!(delete.old_facts.is_some());
    assert!(delete.new_title.is_some());
    assert!(delete.new_facts.is_some());
}

#[tokio::test]
async fn reconciliation_none_bumps_mentions_and_skips_write() {
    let reconciler = Arc::new(QueueReconciler::new(vec![ReconcileDecision::None {
        candidate_index: 0,
    }]));
    let memory = memory_with(None, Some(Arc::clone(&reconciler) as Arc<dyn Reconciler>));

    let first = memory
        .write(sample_obs("incident note", &["observed issue"]))
        .await
        .unwrap();
    assert!(matches!(first, WriteOutcome::Added(_)));

    let second = memory
        .write(sample_obs("incident note", &["same incident"]))
        .await
        .unwrap();
    assert_eq!(second, WriteOutcome::Skipped);

    let search = memory
        .search(&MemoryQuery {
            text: Some("incident note".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(search.len(), 1);
    assert_eq!(search[0].mention_count, 2);

    let history = memory.history(search[0].id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].event, "ADD");
}

#[tokio::test]
async fn vector_search_returns_results() {
    let embedder = Arc::new(RecordingEmbedder::new(8));
    let memory = memory_with(Some(embedder as Arc<dyn EmbeddingProvider>), None);

    assert!(
        memory.vector_search_enabled(),
        "vec0 table should be active when embedder is present"
    );

    memory
        .write(sample_obs("semantic alpha", &["one"]))
        .await
        .unwrap();
    memory
        .write(sample_obs("semantic beta", &["two"]))
        .await
        .unwrap();

    // Verify rows landed in the vec0 virtual table, not just observation_embeddings
    let vec_count: i64 = memory
        .conn
        .lock()
        .expect("memory db mutex poisoned")
        .query_row("SELECT COUNT(*) FROM observations_vec", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(vec_count, 2, "both embeddings should be in the vec0 table");

    let results = memory
        .search(&MemoryQuery {
            text: Some("semantic".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(!results.is_empty());

    // vec0 should still be enabled after the search — no silent fallback
    assert!(
        memory.vector_search_enabled(),
        "vector search should remain enabled after a successful query"
    );
}

#[tokio::test]
async fn search_filters_store() {
    let memory = memory();
    let mut private = sample_obs("private item", &["secret"]);
    private.store = "private".to_owned();
    private.min_trust = min_trust_for_store("private");
    memory.write(private).await.unwrap();

    memory
        .write(sample_obs("shared item", &["visible"]))
        .await
        .unwrap();

    let found = memory
        .search(&MemoryQuery {
            text: Some("item".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].store, "shared");
}

#[tokio::test]
async fn timeline_returns_chronological_window() {
    let memory = memory();
    let one = memory.write(sample_obs("one", &["1"])).await.unwrap();
    let WriteOutcome::Added(one) = one else {
        unreachable!();
    };

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let two = memory.write(sample_obs("two", &["2"])).await.unwrap();
    let WriteOutcome::Added(two) = two else {
        unreachable!();
    };

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let three = memory.write(sample_obs("three", &["3"])).await.unwrap();
    let WriteOutcome::Added(three) = three else {
        unreachable!();
    };

    let timeline = memory.timeline(two, 1, 1).await.unwrap();
    let ids = timeline.iter().map(|o| o.id).collect::<Vec<_>>();
    assert_eq!(ids, vec![one, two, three]);
}

#[tokio::test]
async fn summarize_session_persists_summary() {
    let memory = memory();
    let mut obs = sample_obs("decision: use sqlite", &["done"]);
    obs.obs_type = "decision".to_owned();
    obs.session_key = Some("coop:main".to_owned());
    memory.write(obs).await.unwrap();

    let summary = memory
        .summarize_session(&SessionKey {
            agent_id: "coop".to_owned(),
            kind: SessionKind::Main,
        })
        .await
        .unwrap();

    assert_eq!(summary.session_key, "coop:main");
    assert_eq!(summary.observation_count, 1);
    assert_eq!(summary.decisions.len(), 1);
}

#[tokio::test]
async fn maintenance_compression_creates_summary_and_expires_originals() {
    let memory = memory();

    let mut original_ids = Vec::new();
    for fact in ["alpha", "beta", "gamma"] {
        let outcome = memory
            .write(sample_obs("release notes", &[fact]))
            .await
            .unwrap();
        let WriteOutcome::Added(id) = outcome else {
            panic!("expected add, got {outcome:?}");
        };
        original_ids.push(id);
    }

    let stale_ms = chrono::Utc::now().timestamp_millis() - (3 * 86_400_000);
    for id in &original_ids {
        set_observation_created_at(&memory, *id, stale_ms);
    }

    let config = MemoryMaintenanceConfig {
        archive_after_days: 30,
        delete_archive_after_days: 365,
        compress_after_days: 1,
        compression_min_cluster_size: 3,
        max_rows_per_run: 200,
    };

    let report = memory.run_maintenance(&config).await.unwrap();
    assert_eq!(report.summary_rows, 1);
    assert_eq!(report.compressed_rows, 3);

    let active = memory
        .search(&MemoryQuery {
            text: Some("release notes".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(active.len(), 1);

    for id in original_ids {
        let obs = memory.get(&[id]).await.unwrap();
        assert!(obs.is_empty());

        let history = memory.history(id).await.unwrap();
        assert!(
            history.iter().any(|entry| entry.event == "COMPRESS"),
            "missing COMPRESS history event for {id}"
        );
    }
}

#[tokio::test]
async fn maintenance_archive_moves_rows_to_archive_table() {
    let memory = memory();
    let outcome = memory
        .write(sample_obs("archive candidate", &["row"]))
        .await
        .unwrap();
    let WriteOutcome::Added(id) = outcome else {
        panic!("expected add, got {outcome:?}");
    };

    let stale_ms = chrono::Utc::now().timestamp_millis() - (3 * 86_400_000);
    set_observation_created_at(&memory, id, stale_ms);

    let config = MemoryMaintenanceConfig {
        archive_after_days: 1,
        delete_archive_after_days: 365,
        compress_after_days: 365,
        compression_min_cluster_size: 10,
        max_rows_per_run: 200,
    };

    let report = memory.run_maintenance(&config).await.unwrap();
    assert_eq!(report.archived_rows, 1);

    let active = memory.get(&[id]).await.unwrap();
    assert!(active.is_empty());
    assert_eq!(archive_row_count(&memory), 1);

    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let archived = conn
        .query_row(
            "SELECT COUNT(*) FROM observation_archive WHERE original_observation_id = ?",
            rusqlite::params![id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    drop(conn);
    assert_eq!(archived, 1);
}

#[tokio::test]
async fn maintenance_archive_cleanup_deletes_old_archive_rows() {
    let memory = memory();
    let outcome = memory
        .write(sample_obs("cleanup candidate", &["row"]))
        .await
        .unwrap();
    let WriteOutcome::Added(id) = outcome else {
        panic!("expected add, got {outcome:?}");
    };

    let stale_ms = chrono::Utc::now().timestamp_millis() - (4 * 86_400_000);
    set_observation_created_at(&memory, id, stale_ms);

    let archive_only = MemoryMaintenanceConfig {
        archive_after_days: 1,
        delete_archive_after_days: 365,
        compress_after_days: 365,
        compression_min_cluster_size: 10,
        max_rows_per_run: 200,
    };
    let archived_report = memory.run_maintenance(&archive_only).await.unwrap();
    assert_eq!(archived_report.archived_rows, 1);
    assert_eq!(archive_row_count(&memory), 1);

    let very_old_archive = chrono::Utc::now().timestamp_millis() - (5 * 86_400_000);
    set_archive_archived_at(&memory, very_old_archive);

    let cleanup_config = MemoryMaintenanceConfig {
        archive_after_days: 365,
        delete_archive_after_days: 1,
        compress_after_days: 365,
        compression_min_cluster_size: 10,
        max_rows_per_run: 200,
    };

    let cleanup_report = memory.run_maintenance(&cleanup_config).await.unwrap();
    assert_eq!(cleanup_report.archive_deleted_rows, 1);
    assert_eq!(archive_row_count(&memory), 0);
}

#[tokio::test]
async fn maintenance_respects_max_rows_per_run() {
    let memory = memory();

    let stale_ms = chrono::Utc::now().timestamp_millis() - (3 * 86_400_000);
    for idx in 0..4 {
        let outcome = memory
            .write(sample_obs(&format!("bounded archive {idx}"), &["row"]))
            .await
            .unwrap();
        let WriteOutcome::Added(id) = outcome else {
            panic!("expected add, got {outcome:?}");
        };
        set_observation_created_at(&memory, id, stale_ms);
    }

    let config = MemoryMaintenanceConfig {
        archive_after_days: 1,
        delete_archive_after_days: 365,
        compress_after_days: 365,
        compression_min_cluster_size: 10,
        max_rows_per_run: 2,
    };

    let report = memory.run_maintenance(&config).await.unwrap();
    assert_eq!(report.archived_rows, 2);
    assert_eq!(archive_row_count(&memory), 2);

    let remaining = memory
        .search(&MemoryQuery {
            text: Some("bounded archive".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(remaining.len(), 2);
}

#[test]
fn sqlite_vec_extension_loaded() {
    let embedder = Arc::new(RecordingEmbedder::new(8));
    let memory = memory_with(Some(embedder as Arc<dyn EmbeddingProvider>), None);

    let conn = memory.conn.lock().expect("memory db mutex poisoned");
    let version: String = conn
        .query_row("SELECT vec_version()", [], |row| row.get(0))
        .unwrap();
    assert!(
        version.starts_with('v'),
        "unexpected vec_version: {version}"
    );

    // vec0 virtual table should exist and be usable
    conn.execute(
        "INSERT INTO observations_vec(rowid, embedding) VALUES (999, ?)",
        rusqlite::params![serde_json::to_string(&vec![0.1_f32; 8]).unwrap()],
    )
    .unwrap();

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM observations_vec", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(count >= 1);

    conn.execute("DELETE FROM observations_vec WHERE rowid = 999", [])
        .unwrap();
    drop(conn);
}

#[test]
fn trust_roundtrip_helpers() {
    assert_eq!(trust_from_str("full"), TrustLevel::Full);
    assert_eq!(trust_to_str(TrustLevel::Inner), "inner");
    assert_eq!(min_trust_for_store("private"), TrustLevel::Full);
}
