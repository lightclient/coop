#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use coop_core::fakes::FakeProvider;
use coop_core::tools::DefaultExecutor;
use coop_core::{Provider, ToolExecutor, TrustLevel};
use coop_memory::{Memory, MemoryQuery, SqliteMemory};
use tokio::sync::mpsc;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

#[path = "../src/compaction.rs"]
mod compaction;
#[path = "../src/compaction_store.rs"]
mod compaction_store;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/gateway.rs"]
mod gateway;
#[allow(dead_code)]
#[path = "../src/group_history.rs"]
mod group_history;
#[allow(dead_code)]
#[path = "../src/group_trigger.rs"]
mod group_trigger;
#[path = "../src/memory_auto_capture.rs"]
mod memory_auto_capture;
#[path = "../src/memory_prompt_index.rs"]
mod memory_prompt_index;
#[allow(dead_code)]
#[path = "../src/provider_registry.rs"]
mod provider_registry;
#[allow(dead_code)]
#[path = "../src/session_store.rs"]
mod session_store;

use config::{Config, shared_config};
use gateway::Gateway;

struct AutoCaptureHarness {
    _dir: tempfile::TempDir,
    gateway: Arc<Gateway>,
    memory: Arc<SqliteMemory>,
}

impl AutoCaptureHarness {
    fn new(config: Config, provider: Arc<dyn Provider>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "You are a test agent.").unwrap();

        let db_path = dir.path().join("memory.db");
        let memory = Arc::new(SqliteMemory::open(&db_path, "coop").unwrap());
        let memory_dyn: Arc<dyn Memory> = Arc::<SqliteMemory>::clone(&memory);
        let executor: Arc<dyn ToolExecutor> = Arc::new(DefaultExecutor::new());

        let gateway = Arc::new(
            Gateway::new(
                shared_config(config),
                workspace,
                provider_registry::ProviderRegistry::new(provider),
                executor,
                None,
                Some(memory_dyn),
            )
            .unwrap(),
        );

        Self {
            _dir: dir,
            gateway,
            memory,
        }
    }

    async fn run_turn(&self, trust: TrustLevel) {
        let session_key = self.gateway.default_session_key();
        let (tx, mut rx) = mpsc::channel(32);
        self.gateway
            .run_turn_with_trust(&session_key, "hello", trust, Some("alice"), None, tx)
            .await
            .unwrap();

        while rx.try_recv().is_ok() {}
    }
}

fn config_with_auto_capture(enabled: bool, min_turn_messages: usize) -> Config {
    toml::from_str(&format!(
        r#"
[agent]
id = "coop"
model = "fake-model"

[memory.prompt_index]
enabled = false

[memory.auto_capture]
enabled = {enabled}
min_turn_messages = {min_turn_messages}
"#
    ))
    .unwrap()
}

static TRACE_INIT: OnceLock<()> = OnceLock::new();
static TRACE_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
const TRACE_FILE_PATH: &str = "/tmp/coop-memory-auto-capture-trace.jsonl";

fn ensure_global_trace_subscriber() -> &'static Path {
    let trace_path = TRACE_PATH.get_or_init(|| {
        std::env::var("COOP_TRACE_FILE").map_or_else(
            |_| std::path::PathBuf::from(TRACE_FILE_PATH),
            std::path::PathBuf::from,
        )
    });

    TRACE_INIT.get_or_init(|| {
        if let Some(parent) = trace_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let event_writer_path = trace_path.clone();
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(move || {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&event_writer_path)
                    .unwrap()
            })
            .with_span_list(true)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::Registry::default().with(layer),
        );
    });

    trace_path.as_path()
}

async fn wait_for_observation(memory: &SqliteMemory, title_fragment: &str) -> bool {
    for _ in 0..40 {
        let rows = memory
            .search(&MemoryQuery {
                stores: vec!["shared".to_owned()],
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();

        if rows.iter().any(|row| row.title.contains(title_fragment)) {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    false
}

#[tokio::test]
async fn auto_capture_writes_observations_after_turn() {
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
        r#"[{"title":"Captured build discovery","narrative":"Fixed failing build in turn","facts":["cargo fmt ran"],"type":"discovery","tags":["build"],"related_people":["alice"],"related_files":["./crates\\coop-gateway//src/gateway.rs"]}]"#,
    ));

    let harness = AutoCaptureHarness::new(config_with_auto_capture(true, 1), provider);
    harness.run_turn(TrustLevel::Inner).await;

    assert!(
        wait_for_observation(&harness.memory, "Captured build discovery").await,
        "expected auto-capture observation to be written"
    );

    let rows = harness
        .memory
        .search(&MemoryQuery {
            text: Some("Captured build discovery".to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 5,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let observations = harness.memory.get(&[rows[0].id]).await.unwrap();
    assert_eq!(
        observations[0].related_files,
        vec!["crates/coop-gateway/src/gateway.rs"]
    );
}

#[tokio::test]
async fn auto_capture_is_skipped_when_disabled() {
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
        r#"[{"title":"Should not persist","narrative":"disabled","facts":["x"],"type":"event","tags":[],"related_people":[],"related_files":[]}]"#,
    ));

    let harness = AutoCaptureHarness::new(config_with_auto_capture(false, 1), provider);
    harness.run_turn(TrustLevel::Inner).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let rows = harness
        .memory
        .search(&MemoryQuery {
            stores: vec!["shared".to_owned()],
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(rows.is_empty(), "auto-capture should be disabled");
}

#[tokio::test]
async fn auto_capture_is_skipped_for_short_turns() {
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
        r#"[{"title":"Should not persist","narrative":"too short","facts":["x"],"type":"event","tags":[],"related_people":[],"related_files":[]}]"#,
    ));

    let harness = AutoCaptureHarness::new(config_with_auto_capture(true, 3), provider);
    harness.run_turn(TrustLevel::Inner).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let rows = harness
        .memory
        .search(&MemoryQuery {
            stores: vec!["shared".to_owned()],
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();

    assert!(rows.is_empty(), "short turns should skip auto-capture");
}

#[tokio::test]
async fn trace_contains_auto_capture_metadata_only() {
    let trace_path = ensure_global_trace_subscriber();
    let _ = std::fs::remove_file(trace_path);

    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new(
        r#"[{"title":"Captured trace-safe discovery","narrative":"metadata only","facts":["cargo test"],"type":"discovery","tags":["trace"],"related_people":["alice"],"related_files":["crates/coop-gateway/src/memory_auto_capture.rs"]}]"#,
    ));

    let harness = AutoCaptureHarness::new(config_with_auto_capture(true, 1), provider);
    harness.run_turn(TrustLevel::Inner).await;

    for _ in 0..40 {
        let trace = std::fs::read_to_string(trace_path).unwrap_or_default();
        if trace.contains("post-turn auto-capture complete") {
            assert!(
                !trace.contains("Captured trace-safe discovery"),
                "trace should not include observation content"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let trace = std::fs::read_to_string(trace_path).unwrap_or_default();
    panic!(
        "expected auto-capture trace metadata in {}\n{}",
        trace_path.display(),
        trace
    );
}
