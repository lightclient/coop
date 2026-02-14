#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;

use coop_core::TrustLevel;
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_memory::{Memory, NewObservation, SqliteMemory, WriteOutcome, min_trust_for_store};

#[path = "../src/memory_tools.rs"]
mod memory_tools;

use memory_tools::MemoryToolExecutor;

struct Harness {
    _dir: tempfile::TempDir,
    workspace: PathBuf,
    memory: Arc<SqliteMemory>,
    executor: MemoryToolExecutor,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let db_path = dir.path().join("memory.db");
        let memory = Arc::new(SqliteMemory::open(&db_path, "coop").unwrap());
        let executor =
            MemoryToolExecutor::new(Arc::<SqliteMemory>::clone(&memory) as Arc<dyn Memory>);

        Self {
            _dir: dir,
            workspace,
            memory,
            executor,
        }
    }

    fn ctx(&self, trust: TrustLevel) -> ToolContext {
        ToolContext {
            session_id: "coop:main".to_owned(),
            trust,
            workspace: self.workspace.clone(),
            user_name: None,
        }
    }

    async fn write_observation(&self, store: &str, title: &str, related_files: Vec<&str>) -> i64 {
        let outcome = self
            .memory
            .write(NewObservation {
                session_key: Some("coop:main".to_owned()),
                store: store.to_owned(),
                obs_type: "technical".to_owned(),
                title: title.to_owned(),
                narrative: String::new(),
                facts: vec![],
                tags: vec![],
                source: "test".to_owned(),
                related_files: related_files.into_iter().map(str::to_owned).collect(),
                related_people: vec![],
                token_count: Some(8),
                expires_at: None,
                min_trust: min_trust_for_store(store),
            })
            .await
            .unwrap();

        match outcome {
            WriteOutcome::Added(id) => id,
            other => panic!("expected added outcome, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn memory_files_tool_trust_gating() {
    let harness = Harness::new();

    harness
        .write_observation("private", "private file note", vec!["src/main.rs"])
        .await;
    harness
        .write_observation("social", "social file note", vec!["src/main.rs"])
        .await;

    let output = harness
        .executor
        .execute(
            "memory_files",
            serde_json::json!({"path": "src/main.rs"}),
            &harness.ctx(TrustLevel::Familiar),
        )
        .await
        .unwrap();

    assert!(!output.is_error);
    let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["results"][0]["store"], "social");
}

#[tokio::test]
async fn memory_files_tool_check_exists() {
    let harness = Harness::new();
    std::fs::write(harness.workspace.join("present.rs"), "fn present() {}\n").unwrap();

    harness
        .write_observation(
            "shared",
            "file existence note",
            vec!["present.rs", "missing.rs"],
        )
        .await;

    let output = harness
        .executor
        .execute(
            "memory_files",
            serde_json::json!({
                "path": "present.rs",
                "check_exists": true
            }),
            &harness.ctx(TrustLevel::Inner),
        )
        .await
        .unwrap();

    assert!(!output.is_error);
    let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    let files = payload["results"][0]["files"].as_array().unwrap();

    let present = files
        .iter()
        .find(|entry| entry["path"] == "present.rs")
        .unwrap();
    let missing = files
        .iter()
        .find(|entry| entry["path"] == "missing.rs")
        .unwrap();

    assert_eq!(present["exists"], true);
    assert_eq!(missing["exists"], false);
}

#[tokio::test]
async fn memory_search_file_filter() {
    let harness = Harness::new();

    let keep_id = harness
        .write_observation("shared", "deployment note one", vec!["./src//main.rs"])
        .await;
    harness
        .write_observation("shared", "deployment note two", vec!["src/lib.rs"])
        .await;

    let output = harness
        .executor
        .execute(
            "memory_search",
            serde_json::json!({
                "query": "deployment",
                "file": "src/main.rs"
            }),
            &harness.ctx(TrustLevel::Inner),
        )
        .await
        .unwrap();

    assert!(!output.is_error);
    let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(payload["count"], 1);
    assert_eq!(payload["results"][0]["id"], keep_id);
}
