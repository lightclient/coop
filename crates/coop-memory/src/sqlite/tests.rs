#![allow(clippy::unwrap_used)]

use crate::traits::Memory;
use crate::types::{
    MemoryQuery, NewObservation, WriteOutcome, min_trust_for_store, trust_from_str, trust_to_str,
};
use coop_core::{SessionKey, SessionKind, TrustLevel};

use super::SqliteMemory;

fn memory() -> SqliteMemory {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let memory = SqliteMemory::open(path, "coop").unwrap();
    std::mem::forget(dir);
    memory
}

fn sample_obs(title: &str, store: &str) -> NewObservation {
    NewObservation {
        session_key: Some("coop:main".to_owned()),
        store: store.to_owned(),
        obs_type: "technical".to_owned(),
        title: title.to_owned(),
        narrative: format!("narrative for {title}"),
        facts: vec![format!("fact for {title}")],
        tags: vec!["test".to_owned()],
        source: "agent".to_owned(),
        related_files: vec!["src/main.rs".to_owned()],
        related_people: vec!["alice".to_owned()],
        token_count: Some(50),
        expires_at: None,
        min_trust: min_trust_for_store(store),
    }
}

#[tokio::test]
async fn write_and_get_round_trip() {
    let memory = memory();
    let outcome = memory.write(sample_obs("first", "shared")).await.unwrap();
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
    let first = memory.write(sample_obs("dup", "shared")).await.unwrap();
    assert!(matches!(first, WriteOutcome::Added(_)));

    let second = memory.write(sample_obs("dup", "shared")).await.unwrap();
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
async fn search_filters_store() {
    let memory = memory();
    memory
        .write(sample_obs("private item", "private"))
        .await
        .unwrap();
    memory
        .write(sample_obs("shared item", "shared"))
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
    let one = memory.write(sample_obs("one", "shared")).await.unwrap();
    let WriteOutcome::Added(one) = one else {
        unreachable!();
    };

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let two = memory.write(sample_obs("two", "shared")).await.unwrap();
    let WriteOutcome::Added(two) = two else {
        unreachable!();
    };

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let three = memory.write(sample_obs("three", "shared")).await.unwrap();
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
    let mut obs = sample_obs("decision: use sqlite", "shared");
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

#[test]
fn trust_roundtrip_helpers() {
    assert_eq!(trust_from_str("full"), TrustLevel::Full);
    assert_eq!(trust_to_str(TrustLevel::Inner), "inner");
    assert_eq!(min_trust_for_store("private"), TrustLevel::Full);
}
