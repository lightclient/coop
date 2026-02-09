#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_core::tools::{CompositeExecutor, DefaultExecutor};
use coop_core::traits::ProviderStream;
use coop_core::{
    Content, Message, ModelInfo, Provider, SessionKey, ToolDef, ToolExecutor, TrustLevel,
    TurnEvent, Usage,
};
use coop_memory::{Memory, MemoryQuery, SqliteMemory};
use serde_json::Value;
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
#[path = "../src/memory_prompt_index.rs"]
mod memory_prompt_index;
#[path = "../src/memory_reconcile.rs"]
mod memory_reconcile;
#[path = "../src/memory_tools.rs"]
mod memory_tools;
#[path = "../src/session_store.rs"]
mod session_store;

use config::Config;
use gateway::Gateway;
use memory_reconcile::ProviderReconciler;
use memory_tools::MemoryToolExecutor;

#[derive(Debug)]
struct ScriptedProvider {
    model: ModelInfo,
    complete_queue: Mutex<VecDeque<Message>>,
    complete_fast_queue: Mutex<VecDeque<String>>,
    complete_calls: AtomicUsize,
    complete_fast_calls: AtomicUsize,
}

impl ScriptedProvider {
    fn new() -> Self {
        Self {
            model: ModelInfo {
                name: "scripted-model".to_owned(),
                context_limit: 128_000,
            },
            complete_queue: Mutex::new(VecDeque::new()),
            complete_fast_queue: Mutex::new(VecDeque::new()),
            complete_calls: AtomicUsize::new(0),
            complete_fast_calls: AtomicUsize::new(0),
        }
    }

    fn queue_turn_memory_write(&self, arguments: Value) {
        self.complete_queue
            .lock()
            .unwrap()
            .push_back(Message::assistant().with_tool_request(
                "tool-memory-write",
                "memory_write",
                arguments,
            ));
        self.complete_queue
            .lock()
            .unwrap()
            .push_back(Message::assistant().with_text("ack"));
    }

    fn queue_reconciliation_json(&self, json: &Value) {
        self.complete_fast_queue
            .lock()
            .unwrap()
            .push_back(json.to_string());
    }

    fn complete_fast_calls(&self) -> usize {
        self.complete_fast_calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn model_info(&self) -> &ModelInfo {
        &self.model
    }

    async fn complete(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.complete_calls.fetch_add(1, Ordering::Relaxed);
        let next = self
            .complete_queue
            .lock()
            .unwrap()
            .pop_front()
            .context("scripted provider complete queue exhausted")?;
        Ok((next, Usage::default()))
    }

    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        anyhow::bail!("scripted provider does not support streaming")
    }

    async fn complete_fast(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.complete_fast_calls.fetch_add(1, Ordering::Relaxed);
        let text = self
            .complete_fast_queue
            .lock()
            .unwrap()
            .pop_front()
            .context("scripted provider complete_fast queue exhausted")?;
        Ok((Message::assistant().with_text(text), Usage::default()))
    }
}

struct GatewayHarness {
    _dir: tempfile::TempDir,
    gateway: Arc<Gateway>,
    memory: Arc<SqliteMemory>,
    provider: Arc<ScriptedProvider>,
    session_key: SessionKey,
}

impl GatewayHarness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "You are a test agent.").unwrap();

        let provider = Arc::new(ScriptedProvider::new());
        let provider_dyn: Arc<dyn Provider> = Arc::<ScriptedProvider>::clone(&provider);

        let reconciler = Arc::new(ProviderReconciler::new(Arc::<dyn Provider>::clone(
            &provider_dyn,
        )));
        let db_path = dir.path().join("memory.db");
        let memory = Arc::new(
            SqliteMemory::open_with_components(
                &db_path,
                "coop",
                None,
                Some(reconciler as Arc<dyn coop_memory::Reconciler>),
            )
            .unwrap(),
        );

        let memory_dyn: Arc<dyn Memory> = Arc::<SqliteMemory>::clone(&memory);
        let executor: Arc<dyn ToolExecutor> = Arc::new(CompositeExecutor::new(vec![
            Box::new(DefaultExecutor::new()),
            Box::new(MemoryToolExecutor::new(Arc::<dyn Memory>::clone(
                &memory_dyn,
            ))),
        ]));

        let gateway = Arc::new(
            Gateway::new(
                test_config(),
                workspace,
                provider_dyn,
                executor,
                None,
                Some(memory_dyn),
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();

        Self {
            _dir: dir,
            gateway,
            memory,
            provider,
            session_key,
        }
    }

    async fn run_turn(&self, user_input: &str, trust: TrustLevel) -> Vec<TurnEvent> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        self.gateway
            .run_turn_with_trust(
                &self.session_key,
                user_input,
                trust,
                Some("alice"),
                None,
                event_tx,
            )
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        events
    }
}

fn test_config() -> Config {
    serde_yaml::from_str(
        "
agent:
  id: coop
  model: scripted-model
",
    )
    .unwrap()
}

static TRACE_INIT: OnceLock<()> = OnceLock::new();
static TRACE_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
const TRACE_FILE_PATH: &str = "/tmp/coop-memory-reconciliation-e2e-trace.jsonl";

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
            .with_file(true)
            .with_line_number(true)
            .with_filter(tracing_subscriber::EnvFilter::new("debug"));

        let subscriber = tracing_subscriber::Registry::default().with(layer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber already set");
    });

    trace_path.as_path()
}

fn write_args(title: &str, facts: &[&str]) -> Value {
    serde_json::json!({
        "store": "shared",
        "type": "technical",
        "title": title,
        "narrative": format!("narrative for {title}"),
        "facts": facts,
        "tags": ["test"],
        "related_files": ["src/main.rs"],
        "related_people": ["alice"]
    })
}

fn reconcile_update(title: &str, facts: &[&str]) -> Value {
    serde_json::json!({
        "decision": "UPDATE",
        "candidate_index": 0,
        "merged": {
            "store": "shared",
            "obs_type": "technical",
            "title": title,
            "narrative": format!("merged narrative for {title}"),
            "facts": facts,
            "tags": ["merged"],
            "related_files": ["src/lib.rs"],
            "related_people": ["alice"]
        }
    })
}

fn reconcile_delete() -> Value {
    serde_json::json!({
        "decision": "DELETE",
        "candidate_index": 0,
        "merged": null
    })
}

fn reconcile_none() -> Value {
    serde_json::json!({
        "decision": "NONE",
        "candidate_index": 0,
        "merged": null
    })
}

fn first_tool_result(events: &[TurnEvent]) -> (String, bool) {
    events
        .iter()
        .find_map(|event| {
            if let TurnEvent::ToolResult { message, .. } = event {
                message.content.iter().find_map(|content| {
                    if let Content::ToolResult {
                        output, is_error, ..
                    } = content
                    {
                        Some((output.clone(), *is_error))
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        })
        .context("missing tool result event")
        .unwrap()
}

fn first_tool_result_json(events: &[TurnEvent]) -> Value {
    let (output, is_error) = first_tool_result(events);
    assert!(!is_error, "tool result should be success: {output}");
    serde_json::from_str(&output).unwrap()
}

async fn search_shared(memory: &SqliteMemory, query: &str) -> Vec<coop_memory::ObservationIndex> {
    memory
        .search(&MemoryQuery {
            text: Some(query.to_owned()),
            stores: vec!["shared".to_owned()],
            limit: 20,
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn add_path_records_added_outcome_and_history() {
    let harness = GatewayHarness::new();

    harness
        .provider
        .queue_turn_memory_write(write_args("deploy checklist", &["build", "test"]));

    let events = harness.run_turn("save memory", TrustLevel::Full).await;
    let payload = first_tool_result_json(&events);

    assert_eq!(payload["outcome"], "added");
    let id = payload["id"].as_i64().unwrap();

    let history = harness.memory.history(id).await.unwrap();
    let history_events = history
        .iter()
        .map(|entry| entry.event.clone())
        .collect::<Vec<_>>();
    assert_eq!(history_events, vec!["ADD"]);

    let rows = search_shared(&harness.memory, "deploy checklist").await;
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn update_path_mutates_row_and_records_update_history() {
    let harness = GatewayHarness::new();

    harness
        .provider
        .queue_turn_memory_write(write_args("service status", &["healthy"]));
    let seed_events = harness.run_turn("seed", TrustLevel::Full).await;
    let seed_payload = first_tool_result_json(&seed_events);
    let seed_id = seed_payload["id"].as_i64().unwrap();

    harness
        .provider
        .queue_reconciliation_json(&reconcile_update("service status", &["healthy", "stable"]));
    harness
        .provider
        .queue_turn_memory_write(write_args("service status", &["stable"]));

    let events = harness.run_turn("update", TrustLevel::Full).await;
    let payload = first_tool_result_json(&events);
    assert_eq!(payload["outcome"], "updated");
    assert_eq!(payload["id"].as_i64(), Some(seed_id));

    let current = harness.memory.get(&[seed_id]).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].facts, vec!["healthy", "stable"]);

    let history = harness.memory.history(seed_id).await.unwrap();
    let history_events = history
        .iter()
        .map(|entry| entry.event.clone())
        .collect::<Vec<_>>();
    assert_eq!(history_events, vec!["ADD", "UPDATE"]);

    assert_eq!(harness.provider.complete_fast_calls(), 1);
}

#[tokio::test]
async fn delete_path_expires_old_row_and_inserts_replacement() {
    let harness = GatewayHarness::new();

    harness
        .provider
        .queue_turn_memory_write(write_args("rotation key", &["v1"]));
    let seed_events = harness.run_turn("seed", TrustLevel::Full).await;
    let seed_payload = first_tool_result_json(&seed_events);
    let old_id = seed_payload["id"].as_i64().unwrap();

    harness
        .provider
        .queue_reconciliation_json(&reconcile_delete());
    harness
        .provider
        .queue_turn_memory_write(write_args("rotation key", &["v2"]));

    let events = harness.run_turn("rotate", TrustLevel::Full).await;
    let payload = first_tool_result_json(&events);
    assert_eq!(payload["outcome"], "deleted");
    assert_eq!(payload["id"].as_i64(), Some(old_id));

    let old = harness.memory.get(&[old_id]).await.unwrap();
    assert!(old.is_empty(), "deleted row should be inaccessible");

    let rows = search_shared(&harness.memory, "rotation key").await;
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].id, old_id, "replacement row should be new id");

    let history = harness.memory.history(old_id).await.unwrap();
    let history_events = history
        .iter()
        .map(|entry| entry.event.clone())
        .collect::<Vec<_>>();
    assert_eq!(history_events, vec!["ADD", "DELETE"]);
}

#[tokio::test]
async fn none_path_bumps_mentions_and_skips_new_insert() {
    let harness = GatewayHarness::new();

    harness
        .provider
        .queue_turn_memory_write(write_args("incident note", &["observed issue"]));
    let _ = harness.run_turn("seed", TrustLevel::Full).await;

    harness
        .provider
        .queue_reconciliation_json(&reconcile_none());
    harness
        .provider
        .queue_turn_memory_write(write_args("incident note", &["same mention"]));

    let events = harness.run_turn("repeat", TrustLevel::Full).await;
    let payload = first_tool_result_json(&events);
    assert_eq!(payload["outcome"], "skipped");

    let rows = search_shared(&harness.memory, "incident note").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].mention_count, 2);

    let history = harness.memory.history(rows[0].id).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].event, "ADD");
}

#[tokio::test]
async fn exact_dup_short_circuits_without_reconciler_call() {
    let harness = GatewayHarness::new();

    harness
        .provider
        .queue_turn_memory_write(write_args("same facts", &["x", "y"]));
    let _ = harness.run_turn("first", TrustLevel::Full).await;

    harness
        .provider
        .queue_turn_memory_write(write_args("same facts", &["x", "y"]));
    let events = harness.run_turn("duplicate", TrustLevel::Full).await;
    let payload = first_tool_result_json(&events);
    assert_eq!(payload["outcome"], "exact_dup");

    let rows = search_shared(&harness.memory, "same facts").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].mention_count, 2);

    assert_eq!(harness.provider.complete_fast_calls(), 0);
}

#[tokio::test]
async fn trust_gate_rejects_inaccessible_store_without_reconciliation() {
    let harness = GatewayHarness::new();

    harness.provider.queue_turn_memory_write(serde_json::json!({
        "store": "private",
        "type": "technical",
        "title": "private detail",
        "facts": ["secret"]
    }));

    let events = harness.run_turn("private", TrustLevel::Inner).await;
    let (output, is_error) = first_tool_result(&events);

    assert!(is_error);
    assert!(output.contains("not accessible"));
    assert_eq!(harness.provider.complete_fast_calls(), 0);

    let rows = harness
        .memory
        .search(&MemoryQuery {
            text: Some("private detail".to_owned()),
            stores: vec![
                "private".to_owned(),
                "shared".to_owned(),
                "social".to_owned(),
            ],
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn trace_contains_reconciliation_request_decision_and_apply_events() {
    let trace_file = ensure_global_trace_subscriber().to_path_buf();
    std::fs::write(&trace_file, "").unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let harness = GatewayHarness::new();

        harness
            .provider
            .queue_turn_memory_write(write_args("trace target", &["one"]));
        let _ = harness.run_turn("seed", TrustLevel::Full).await;

        harness
            .provider
            .queue_reconciliation_json(&reconcile_update("trace target", &["one", "two"]));
        harness
            .provider
            .queue_turn_memory_write(write_args("trace target", &["two"]));
        let _ = harness.run_turn("update", TrustLevel::Full).await;

        assert_eq!(harness.provider.complete_fast_calls(), 1);
    });

    let trace = std::fs::read_to_string(&trace_file).unwrap();

    assert!(
        trace.contains("memory reconciliation request"),
        "missing reconciliation request event"
    );
    assert!(
        trace.contains("memory reconciliation decision")
            || trace.contains("memory reconciliation provider decision"),
        "missing reconciliation decision event"
    );
    assert!(
        trace.contains("memory reconciliation applied: UPDATE"),
        "missing reconciliation applied event"
    );
}
