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
#[path = "../src/memory_auto_capture.rs"]
mod memory_auto_capture;
#[path = "../src/memory_prompt_index.rs"]
mod memory_prompt_index;
#[path = "../src/session_store.rs"]
mod session_store;

use config::{Config, shared_config};
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

    fn model_info(&self) -> ModelInfo {
        self.model.clone()
    }

    async fn complete(
        &self,
        system: &[String],
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.seen_system_prompts
            .lock()
            .unwrap()
            .push(system.join("\n\n"));
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
        _system: &[String],
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
                shared_config(config),
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
                shared_config(config),
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
        self.run_turn_with_input("hello", trust).await
    }

    async fn run_turn_with_input(&self, input: &str, trust: TrustLevel) -> Vec<TurnEvent> {
        let (event_tx, mut event_rx) = mpsc::channel(32);
        self.gateway
            .run_turn_with_trust(
                &self.session_key,
                input,
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

fn config_with_prompt_index(
    enabled: bool,
    limit: usize,
    max_tokens: usize,
    recent_days: u32,
) -> Config {
    toml::from_str(&format!(
        r#"
[agent]
id = "coop"
model = "prompt-capture-model"

[memory.prompt_index]
enabled = {enabled}
limit = {limit}
max_tokens = {max_tokens}
recent_days = {recent_days}
"#
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

async fn seed(memory: &SqliteMemory, store: &str, title: &str) -> i64 {
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
    match result {
        WriteOutcome::Added(id) => id,
        other => panic!("expected add, got {other:?}"),
    }
}

async fn wait_for_summary_count(memory: &SqliteMemory, expected: usize) -> Vec<SessionSummary> {
    for _ in 0..40 {
        let summaries = memory.recent_session_summaries(10).await.unwrap();
        if summaries.len() == expected {
            return summaries;
        }
        sleep(Duration::from_millis(25)).await;
    }

    memory.recent_session_summaries(10).await.unwrap()
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

    async fn recent_session_summaries(&self, _limit: usize) -> Result<Vec<SessionSummary>> {
        Ok(Vec::new())
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

#[derive(Debug)]
struct ScriptedMemory {
    recent: Vec<ObservationIndex>,
    relevance: Vec<ObservationIndex>,
}

impl ScriptedMemory {
    fn new(recent: Vec<ObservationIndex>, relevance: Vec<ObservationIndex>) -> Self {
        Self { recent, relevance }
    }
}

#[async_trait]
impl Memory for ScriptedMemory {
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ObservationIndex>> {
        let mut rows = if query.after.is_some() && query.text.is_none() {
            self.recent.clone()
        } else if query
            .text
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            self.relevance.clone()
        } else {
            Vec::new()
        };
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
        Ok(Vec::new())
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

fn observation_index(id: i64, title: &str, days_ago: i64, mention_count: u32) -> ObservationIndex {
    ObservationIndex {
        id,
        title: title.to_owned(),
        obs_type: "technical".to_owned(),
        store: "shared".to_owned(),
        created_at: Utc::now() - chrono::Duration::days(days_ago),
        token_count: 32,
        mention_count,
        score: 0.9,
        related_people: vec!["alice".to_owned()],
    }
}

#[tokio::test]
async fn full_trust_injects_memory_prompt_index_when_data_exists() {
    let config = config_with_prompt_index(true, 12, 1200, 3);
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
async fn prompt_index_includes_recent_session_summaries_for_full_trust() {
    let config = config_with_prompt_index(true, 12, 2_000, 3);
    let (harness, memory) = PromptHarness::with_sqlite(config);

    seed(&memory, "shared", "summary seed row").await;
    memory
        .summarize_session(&SessionKey {
            agent_id: "coop".to_owned(),
            kind: coop_core::SessionKind::Main,
        })
        .await
        .unwrap();

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Full).await;

    let prompt = harness.provider.last_system_prompt();
    assert!(prompt.contains("## Recent Sessions"));
    assert!(prompt.contains("session=coop:main"));
}

#[tokio::test]
async fn trace_contains_prompt_index_build_and_injected_events() {
    let trace_path = ensure_global_trace_subscriber();
    let _ = std::fs::remove_file(trace_path);

    let config = config_with_prompt_index(true, 12, 1200, 3);
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
async fn post_turn_summary_write_uses_session_upsert() {
    let config = config_with_prompt_index(true, 12, 1200, 3);
    let (harness, memory) = PromptHarness::with_sqlite(config);

    seed(&memory, "shared", "session summary source row").await;

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Full).await;
    let first = wait_for_summary_count(&memory, 1).await;
    assert_eq!(first[0].session_key, "coop:main");

    harness.provider.queue_text_response("ok");
    let _ = harness.run_turn(TrustLevel::Full).await;
    let second = wait_for_summary_count(&memory, 1).await;

    assert_eq!(
        second.len(),
        1,
        "summary should be upserted, not duplicated"
    );
}

#[tokio::test]
async fn familiar_and_public_trust_gate_prompt_index_content() {
    let config = config_with_prompt_index(true, 12, 1200, 3);
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
    let config = config_with_prompt_index(true, 12, 90, 3);
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
    let config = config_with_prompt_index(true, 12, 1200, 3);
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

#[tokio::test]
async fn query_aware_index_surfaces_relevant_non_recent_observation() {
    let memory: Arc<dyn Memory> = Arc::new(ScriptedMemory::new(
        vec![observation_index(1, "recent standup summary", 1, 1)],
        vec![observation_index(
            2,
            "deployment pipeline uses blue-green strategy",
            30,
            500,
        )],
    ));

    let config = config_with_prompt_index(true, 6, 3_000, 3);
    let block = memory_prompt_index::build_prompt_index(
        memory.as_ref(),
        TrustLevel::Full,
        &config.memory.prompt_index,
        "tell me about the deployment pipeline",
    )
    .await
    .unwrap()
    .unwrap();

    assert!(block.contains("## Memory Index (DB)"));
    assert!(
        block.contains("deployment pipeline"),
        "query-relevant observation should appear in prompt index\n{block}"
    );
}

#[tokio::test]
async fn recent_window_rows_are_rendered_before_older_relevance_hits() {
    let memory: Arc<dyn Memory> = Arc::new(ScriptedMemory::new(
        vec![
            observation_index(1, "recent build status (1 day)", 1, 2),
            observation_index(2, "recent deploy note (2 days)", 2, 1),
        ],
        vec![
            observation_index(3, "old migration decision (30 days)", 30, 999),
            observation_index(4, "mid-age backlog item (5 days)", 5, 25),
        ],
    ));

    let config = config_with_prompt_index(true, 4, 3_000, 3);
    let block = memory_prompt_index::build_prompt_index(
        memory.as_ref(),
        TrustLevel::Full,
        &config.memory.prompt_index,
        "what happened with migration and deploy",
    )
    .await
    .unwrap()
    .unwrap();

    let rows = block
        .lines()
        .filter(|line| line.starts_with("- id="))
        .collect::<Vec<_>>();

    assert!(rows[0].contains("recent build status"));
    assert!(rows[1].contains("recent deploy note"));
    assert!(rows[2].contains("old migration decision"));
}

#[tokio::test]
async fn no_recent_rows_gives_full_slot_budget_to_relevance_hits() {
    let memory: Arc<dyn Memory> = Arc::new(ScriptedMemory::new(
        Vec::new(),
        vec![
            observation_index(10, "relevance hit one", 7, 10),
            observation_index(11, "relevance hit two", 9, 8),
            observation_index(12, "relevance hit three", 12, 6),
        ],
    ));

    let config = config_with_prompt_index(true, 3, 3_000, 3);
    let block = memory_prompt_index::build_prompt_index(
        memory.as_ref(),
        TrustLevel::Full,
        &config.memory.prompt_index,
        "relevance",
    )
    .await
    .unwrap()
    .unwrap();

    let row_count = block
        .lines()
        .filter(|line| line.starts_with("- id="))
        .count();

    assert_eq!(row_count, 3);
    assert!(block.contains("relevance hit one"));
    assert!(block.contains("relevance hit three"));
}

#[tokio::test]
async fn token_budget_prefers_recent_rows_and_truncates_relevance_rows() {
    let memory: Arc<dyn Memory> = Arc::new(ScriptedMemory::new(
        vec![
            observation_index(
                21,
                "recent status alpha alpha alpha alpha alpha alpha alpha alpha",
                1,
                4,
            ),
            observation_index(
                22,
                "recent status beta beta beta beta beta beta beta beta",
                2,
                3,
            ),
            observation_index(
                23,
                "recent status gamma gamma gamma gamma gamma gamma gamma gamma",
                3,
                2,
            ),
        ],
        vec![
            observation_index(30, "relevance hit archived docs", 25, 40),
            observation_index(31, "relevance hit old incident", 30, 55),
        ],
    ));

    let config = config_with_prompt_index(true, 5, 90, 3);
    let block = memory_prompt_index::build_prompt_index(
        memory.as_ref(),
        TrustLevel::Full,
        &config.memory.prompt_index,
        "incident docs",
    )
    .await
    .unwrap()
    .unwrap();

    assert!(block.contains("recent status"));
    assert!(!block.contains("relevance hit archived docs"));
    assert!(!block.contains("relevance hit old incident"));
}

#[tokio::test]
async fn prompt_index_file_enrichment_adds_file_linked_section() {
    let config = config_with_prompt_index(true, 12, 2_000, 3);
    let (harness, memory) = PromptHarness::with_sqlite(config);

    seed(&memory, "shared", "gateway refactor memory").await;

    harness.provider.queue_text_response("ok");
    let _ = harness
        .run_turn_with_input("Please check src/main.rs", TrustLevel::Full)
        .await;

    let prompt = harness.provider.last_system_prompt();
    assert!(prompt.contains("### File-linked observations"));
    assert!(prompt.contains("files=[main.rs]"));
}
