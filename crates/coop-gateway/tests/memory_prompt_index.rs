#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use coop_core::tools::DefaultExecutor;
use coop_core::traits::ProviderStream;
use coop_core::{
    Message, ModelInfo, Provider, SessionKey, ToolDef, ToolExecutor, TrustLevel, TurnEvent, Usage,
};
use coop_memory::{
    Memory, MemoryQuery, NewObservation, Observation, ObservationHistoryEntry, ObservationIndex,
    Person, SessionSummary, SqliteMemory, WriteOutcome, min_trust_for_store,
};
use tokio::sync::mpsc;
use tokio::time::sleep;
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
#[path = "../src/session_store.rs"]
mod session_store;

use config::Config;
use gateway::Gateway;

#[derive(Debug)]
struct PromptCaptureProvider {
    model: ModelInfo,
    queue: Mutex<VecDeque<Message>>,
    seen_system_prompts: Mutex<Vec<String>>,
}

impl PromptCaptureProvider {
    fn new() -> Self {
        Self {
            model: ModelInfo {
                name: "prompt-capture-model".to_owned(),
                context_limit: 128_000,
            },
            queue: Mutex::new(VecDeque::new()),
            seen_system_prompts: Mutex::new(Vec::new()),
        }
    }

    fn queue_text_response(&self, text: &str) {
        self.queue
            .lock()
            .unwrap()
            .push_back(Message::assistant().with_text(text));
    }

    fn last_system_prompt(&self) -> String {
        self.seen_system_prompts
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Provider for PromptCaptureProvider {
    fn name(&self) -> &'static str {
        "prompt-capture"
    }

    fn model_info(&self) -> &ModelInfo {
        &self.model
    }

    async fn complete(
        &self,
        system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.seen_system_prompts
            .lock()
            .unwrap()
            .push(system.to_owned());
        let response = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Message::assistant().with_text("ok"));
        Ok((response, Usage::default()))
    }

    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        anyhow::bail!("streaming not supported")
    }
}

struct PromptHarness {
    _dir: tempfile::TempDir,
    gateway: Arc<Gateway>,
    provider: Arc<PromptCaptureProvider>,
    session_key: SessionKey,
}

impl PromptHarness {
    fn with_sqlite(config: Config) -> (Self, Arc<SqliteMemory>) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "You are a test agent.").unwrap();

        let db_path = dir.path().join("memory.db");
        let sqlite_memory = Arc::new(SqliteMemory::open(&db_path, "coop").unwrap());
        let memory_dyn: Arc<dyn Memory> = Arc::<SqliteMemory>::clone(&sqlite_memory);

        let provider = Arc::new(PromptCaptureProvider::new());
        let provider_dyn: Arc<dyn Provider> = Arc::<PromptCaptureProvider>::clone(&provider);
        let executor: Arc<dyn ToolExecutor> = Arc::new(DefaultExecutor::new());

        let gateway = Arc::new(
            Gateway::new(
                config,
                workspace,
                provider_dyn,
                executor,
                None,
                Some(memory_dyn),
            )
            .unwrap(),
        );

        let session_key = gateway.default_session_key();
        (
            Self {
                _dir: dir,
                gateway,
                provider,
                session_key,
            },
            sqlite_memory,
        )
    }

    fn with_memory(config: Config, memory: Arc<dyn Memory>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "You are a test agent.").unwrap();

        let provider = Arc::new(PromptCaptureProvider::new());
        let provider_dyn: Arc<dyn Provider> = Arc::<PromptCaptureProvider>::clone(&provider);
        let executor: Arc<dyn ToolExecutor> = Arc::new(DefaultExecutor::new());

        let gateway = Arc::new(
            Gateway::new(
                config,
                workspace,
                provider_dyn,
                executor,
                None,
                Some(memory),
            )
            .unwrap(),
        );
        let session_key = gateway.default_session_key();

        Self {
            _dir: dir,
            gateway,
            provider,
            session_key,
        }
    }

    async fn run_turn(&self, trust: TrustLevel) -> Vec<TurnEvent> {
        let (event_tx, mut event_rx) = mpsc::channel(32);
        self.gateway
            .run_turn_with_trust(
                &self.session_key,
                "hello",
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

fn config_with_prompt_index(enabled: bool, limit: usize, max_tokens: usize) -> Config {
    serde_yaml::from_str(&format!(
        "
agent:
  id: coop
  model: prompt-capture-model
memory:
  prompt_index:
    enabled: {enabled}
    limit: {limit}
    max_tokens: {max_tokens}
"
    ))
    .unwrap()
}

static TRACE_INIT: OnceLock<()> = OnceLock::new();
static TRACE_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
const TRACE_FILE_PATH: &str = "/tmp/coop-memory-prompt-index-trace.jsonl";

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

async fn seed(memory: &SqliteMemory, store: &str, title: &str) {
    let obs = NewObservation {
        session_key: Some("coop:main".to_owned()),
        store: store.to_owned(),
        obs_type: "technical".to_owned(),
        title: title.to_owned(),
        narrative: format!("narrative for {title}"),
        facts: vec![format!("fact {title}")],
        tags: vec!["test".to_owned()],
        source: "agent".to_owned(),
        related_files: vec!["src/main.rs".to_owned()],
        related_people: vec!["alice".to_owned()],
        token_count: Some(42),
        expires_at: None,
        min_trust: min_trust_for_store(store),
    };

    let result = memory.write(obs).await.unwrap();
    assert!(matches!(result, WriteOutcome::Added(_)));
}

#[derive(Debug)]
struct FailingSearchMemory;

#[async_trait]
impl Memory for FailingSearchMemory {
    async fn search(&self, _query: &MemoryQuery) -> Result<Vec<ObservationIndex>> {
        anyhow::bail!("prompt index search failed")
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
        anyhow::bail!("not used")
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

    async fn history(&self, _observation_id: i64) -> Result<Vec<ObservationHistoryEntry>> {
        Ok(Vec::new())
    }

    async fn run_maintenance(
        &self,
        _config: &coop_memory::MemoryMaintenanceConfig,
    ) -> Result<coop_memory::MemoryMaintenanceReport> {
        Ok(coop_memory::MemoryMaintenanceReport::default())
    }
}

#[tokio::test]
async fn full_trust_injects_memory_prompt_index_when_data_exists() {
    let config = config_with_prompt_index(true, 12, 1200);
    let (harness, memory) = PromptHarness::with_sqlite(config);

    seed(&memory, "shared", "deployment checklist").await;

    harness.provider.queue_text_response("ok");
    let events = harness.run_turn(TrustLevel::Full).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, TurnEvent::Done(_)))
    );

    let prompt = harness.provider.last_system_prompt();
    assert!(prompt.contains("## Memory Index (DB)"));
    assert!(prompt.contains("deployment checklist"));
}

#[tokio::test]
async fn trace_contains_prompt_index_build_and_injected_events() {
    let trace_path = ensure_global_trace_subscriber();
    let _ = std::fs::remove_file(trace_path);

    let config = config_with_prompt_index(true, 12, 1200);
    let (harness, memory) = PromptHarness::with_sqlite(config);
    seed(&memory, "shared", "index trace row").await;

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Full).await;

    for _ in 0..40 {
        let trace = std::fs::read_to_string(trace_path).unwrap_or_default();
        if trace.contains("memory prompt index built")
            && trace.contains("memory prompt index injected")
        {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }

    let trace = std::fs::read_to_string(trace_path).unwrap_or_default();
    panic!(
        "expected prompt index trace events not found in {}\n{}",
        trace_path.display(),
        trace
    );
}

#[tokio::test]
async fn familiar_and_public_trust_gate_prompt_index_content() {
    let config = config_with_prompt_index(true, 12, 1200);
    let (harness, memory) = PromptHarness::with_sqlite(config);

    seed(&memory, "private", "private credential note").await;
    seed(&memory, "shared", "shared internal note").await;
    seed(&memory, "social", "social meetup note").await;

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Familiar).await;
    let familiar_prompt = harness.provider.last_system_prompt();

    assert!(familiar_prompt.contains("## Memory Index (DB)"));
    assert!(familiar_prompt.contains("social meetup note"));
    assert!(!familiar_prompt.contains("private credential note"));
    assert!(!familiar_prompt.contains("shared internal note"));

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Public).await;
    let public_prompt = harness.provider.last_system_prompt();

    assert!(!public_prompt.contains("## Memory Index (DB)"));
}

#[tokio::test]
async fn prompt_index_token_budget_truncates_output() {
    let config = config_with_prompt_index(true, 12, 90);
    let (harness, memory) = PromptHarness::with_sqlite(config);

    seed(
        &memory,
        "shared",
        "very long deployment memory title alpha alpha alpha alpha alpha",
    )
    .await;
    seed(
        &memory,
        "shared",
        "very long deployment memory title beta beta beta beta beta",
    )
    .await;
    seed(
        &memory,
        "shared",
        "very long deployment memory title gamma gamma gamma gamma gamma",
    )
    .await;

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Full).await;

    let prompt = harness.provider.last_system_prompt();
    assert!(prompt.contains("## Memory Index (DB)"));
    assert!(prompt.contains("truncated to fit token budget"));

    let row_count = prompt
        .lines()
        .filter(|line| line.trim_start().starts_with("- id="))
        .count();
    assert!(row_count < 3, "expected truncation, got {row_count} rows");
}

#[tokio::test]
async fn prompt_index_failures_do_not_break_turn_creation() {
    let config = config_with_prompt_index(true, 12, 1200);
    let failing_memory: Arc<dyn Memory> = Arc::new(FailingSearchMemory);
    let harness = PromptHarness::with_memory(config, failing_memory);

    harness.provider.queue_text_response("ok");
    let events = harness.run_turn(TrustLevel::Full).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, TurnEvent::Done(_)))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::Error(_)))
    );

    let prompt = harness.provider.last_system_prompt();
    assert!(!prompt.contains("## Memory Index (DB)"));
}
