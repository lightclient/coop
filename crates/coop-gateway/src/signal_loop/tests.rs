use super::*;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use coop_channels::{
    MockSignalChannel, SignalAction, SignalQuery, SignalTarget, SignalToolExecutor,
    SignalTypingNotifier,
};
use coop_core::fakes::FakeProvider;
use coop_core::tools::DefaultExecutor;
use coop_core::{
    InboundMessage, Message, ModelInfo, Provider, ProviderStream, SessionKey, SessionKind, ToolDef,
    ToolExecutor, TurnEvent, TypingNotifier, Usage,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::{Config, shared_config};
use crate::gateway::Gateway;
use crate::provider_registry::ProviderRegistry;

#[derive(Debug)]
struct CountingProvider {
    response: String,
    calls: AtomicUsize,
    model: ModelInfo,
}

impl CountingProvider {
    fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            calls: AtomicUsize::new(0),
            model: ModelInfo {
                name: "counting-model".to_owned(),
                context_limit: 128_000,
            },
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for CountingProvider {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn model_info(&self) -> ModelInfo {
        self.model.clone()
    }

    async fn complete(
        &self,
        _system: &[String],
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok((
            Message::assistant().with_text(self.response.clone()),
            Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            },
        ))
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

#[derive(Debug)]
struct ScriptedProvider {
    model: ModelInfo,
    responses: Mutex<VecDeque<Message>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<Message>) -> Self {
        Self {
            model: ModelInfo {
                name: "scripted-model".to_owned(),
                context_limit: 128_000,
            },
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn model_info(&self) -> ModelInfo {
        self.model.clone()
    }

    async fn complete(
        &self,
        _system: &[String],
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("script exhausted"))?;

        Ok((
            response,
            Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            },
        ))
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

fn test_config() -> Config {
    toml::from_str(
        r#"
[agent]
id = "coop"
model = "test-model"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid"]
"#,
    )
    .unwrap()
}

fn dummy_query_tx() -> mpsc::Sender<SignalQuery> {
    let (tx, _rx) = mpsc::channel(1);
    tx
}

fn build_router(
    provider: Arc<dyn Provider>,
    executor: Arc<dyn ToolExecutor>,
    typing_notifier: Option<Arc<dyn TypingNotifier>>,
) -> Arc<MessageRouter> {
    let config = test_config();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("SOUL.md"), "You are a test agent.").unwrap();
    let shared = shared_config(config);
    let gateway = Arc::new(
        Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            ProviderRegistry::new(provider),
            executor,
            typing_notifier,
            None,
        )
        .unwrap(),
    );

    Arc::new(MessageRouter::new(shared, gateway))
}

fn inbound_message(
    kind: InboundKind,
    sender: &str,
    chat_id: Option<&str>,
    is_group: bool,
    reply_to: Option<&str>,
) -> InboundMessage {
    InboundMessage {
        channel: "signal".to_owned(),
        sender: sender.to_owned(),
        content: "hello".to_owned(),
        chat_id: chat_id.map(ToOwned::to_owned),
        is_group,
        timestamp: Utc::now(),
        reply_to: reply_to.map(ToOwned::to_owned),
        kind,
        message_timestamp: Some(1234),
        group_revision: None,
    }
}

async fn collect_events(mut event_rx: mpsc::Receiver<TurnEvent>) -> Vec<TurnEvent> {
    let mut events = Vec::new();

    while let Some(event) = event_rx.recv().await {
        let done = matches!(event, TurnEvent::Done(_));
        events.push(event);
        if done {
            break;
        }
    }

    events
}

async fn wait_for_actions(channel: &mut MockSignalChannel, min_count: usize) -> Vec<SignalAction> {
    let mut actions = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

    loop {
        actions.extend(channel.take_actions());
        if actions.len() >= min_count {
            return actions;
        }

        if tokio::time::Instant::now() >= deadline {
            return actions;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn scripted_signal_reply_provider() -> Arc<dyn Provider> {
    Arc::new(ScriptedProvider::new(vec![
        Message::assistant().with_tool_request(
            "tool-1",
            "signal_reply",
            serde_json::json!({
                "chat_id": "alice-uuid",
                "text": "tool reply",
                "reply_to_timestamp": 42,
                "author_id": "alice-uuid"
            }),
        ),
        Message::assistant().with_text("final response"),
    ]))
}

#[test]
fn should_dispatch_matrix_matches_signal_policy() {
    assert!(!should_dispatch_signal_message(&inbound_message(
        InboundKind::Typing,
        "alice-uuid",
        None,
        false,
        Some("alice-uuid"),
    )));
    assert!(!should_dispatch_signal_message(&inbound_message(
        InboundKind::Receipt,
        "alice-uuid",
        None,
        false,
        Some("alice-uuid"),
    )));

    for kind in [
        InboundKind::Text,
        InboundKind::Reaction,
        InboundKind::Edit,
        InboundKind::Attachment,
    ] {
        assert!(should_dispatch_signal_message(&inbound_message(
            kind,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        )));
    }
}

#[test]
fn signal_reply_target_prefers_reply_to() {
    let inbound = inbound_message(
        InboundKind::Text,
        "alice-uuid",
        Some("deadbeef"),
        true,
        Some("group:override"),
    );

    assert_eq!(
        signal_reply_target(&inbound),
        Some("group:override".to_owned())
    );
}

#[test]
fn signal_reply_target_group_fallback_adds_prefix() {
    let inbound = inbound_message(
        InboundKind::Text,
        "alice-uuid",
        Some("deadbeef"),
        true,
        None,
    );
    assert_eq!(
        signal_reply_target(&inbound),
        Some("group:deadbeef".to_owned())
    );

    let already_prefixed = inbound_message(
        InboundKind::Text,
        "alice-uuid",
        Some("group:deadbeef"),
        true,
        None,
    );
    assert_eq!(
        signal_reply_target(&already_prefixed),
        Some("group:deadbeef".to_owned())
    );
}

#[test]
fn signal_reply_target_dm_fallback_uses_sender() {
    let inbound = inbound_message(InboundKind::Text, "alice-uuid", None, false, None);
    assert_eq!(signal_reply_target(&inbound), Some("alice-uuid".to_owned()));
}

#[tokio::test]
async fn handle_signal_inbound_once_filters_typing_and_receipt() {
    let provider = Arc::new(CountingProvider::new("ignored"));
    let router = build_router(
        Arc::clone(&provider) as Arc<dyn Provider>,
        Arc::new(DefaultExecutor::new()),
        None,
    );

    let mut channel = MockSignalChannel::new();
    channel
        .inject_inbound(inbound_message(
            InboundKind::Typing,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();
    channel
        .inject_inbound(inbound_message(
            InboundKind::Receipt,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();
    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    assert_eq!(provider.calls(), 0);
    assert!(channel.take_outbound().is_empty());
}

#[tokio::test]
async fn handle_signal_inbound_once_dispatches_text_reaction_edit_and_attachment() {
    let provider = Arc::new(CountingProvider::new("ack"));
    let router = build_router(
        Arc::clone(&provider) as Arc<dyn Provider>,
        Arc::new(DefaultExecutor::new()),
        None,
    );

    let mut channel = MockSignalChannel::new();
    for kind in [
        InboundKind::Text,
        InboundKind::Reaction,
        InboundKind::Edit,
        InboundKind::Attachment,
    ] {
        channel
            .inject_inbound(inbound_message(
                kind,
                "alice-uuid",
                None,
                false,
                Some("alice-uuid"),
            ))
            .await
            .unwrap();
    }

    for _ in 0..4 {
        handle_signal_inbound_once(&mut channel, router.as_ref())
            .await
            .unwrap();
    }

    assert_eq!(provider.calls(), 4);

    let outbound = channel.take_outbound();
    assert_eq!(outbound.len(), 4);
    for message in outbound {
        assert_eq!(message.channel, "signal");
        assert_eq!(message.target, "alice-uuid");
        assert_eq!(message.content, "ack");
    }
}

#[tokio::test]
async fn handle_signal_inbound_once_sends_non_empty_response() {
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("assistant reply"));
    let router = build_router(provider, Arc::new(DefaultExecutor::new()), None);

    let mut channel = MockSignalChannel::new();
    channel
        .inject_inbound(inbound_message(
            InboundKind::Text,
            "alice-uuid",
            None,
            false,
            Some("group:reply-target"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    let outbound = channel.take_outbound();
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].channel, "signal");
    assert_eq!(outbound[0].target, "group:reply-target");
    assert_eq!(outbound[0].content, "assistant reply");
}

#[tokio::test]
async fn handle_signal_inbound_once_does_not_send_empty_response() {
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("   "));
    let router = build_router(provider, Arc::new(DefaultExecutor::new()), None);

    let mut channel = MockSignalChannel::new();
    channel
        .inject_inbound(inbound_message(
            InboundKind::Text,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    assert!(channel.take_outbound().is_empty());
}

#[tokio::test]
async fn handle_signal_inbound_once_executes_signal_reply_tool() {
    let mut channel = MockSignalChannel::new();
    let provider = scripted_signal_reply_provider();
    let executor: Arc<dyn ToolExecutor> = Arc::new(SignalToolExecutor::new(
        channel.action_sender(),
        dummy_query_tx(),
    ));
    let router = build_router(provider, executor, None);

    channel
        .inject_inbound(inbound_message(
            InboundKind::Text,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    let actions = channel.take_actions();
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        SignalAction::Reply {
            target: SignalTarget::Direct(target),
            text,
            quote_timestamp: 42,
            quote_author_aci,
        } if target == "alice-uuid" && text == "tool reply" && quote_author_aci == "alice-uuid"
    ));

    let outbound = channel.take_outbound();
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].target, "alice-uuid");
    assert_eq!(outbound[0].content, "final response");
}

#[tokio::test]
async fn router_dispatch_emits_tool_events_and_queues_signal_action() {
    let mut channel = MockSignalChannel::new();
    let provider = scripted_signal_reply_provider();
    let executor: Arc<dyn ToolExecutor> = Arc::new(SignalToolExecutor::new(
        channel.action_sender(),
        dummy_query_tx(),
    ));
    let router = build_router(provider, executor, None);

    let inbound = inbound_message(
        InboundKind::Text,
        "alice-uuid",
        None,
        false,
        Some("alice-uuid"),
    );
    let (event_tx, event_rx) = mpsc::channel(64);

    router.dispatch(&inbound, event_tx).await.unwrap();
    let events = collect_events(event_rx).await;

    let saw_tool_start = events.iter().any(|event| {
        matches!(
            event,
            TurnEvent::ToolStart {
                id,
                name,
                ..
            } if id == "tool-1" && name == "signal_reply"
        )
    });
    let saw_tool_result = events
        .iter()
        .any(|event| matches!(event, TurnEvent::ToolResult { id, .. } if id == "tool-1"));

    assert!(saw_tool_start);
    assert!(saw_tool_result);

    let actions = channel.take_actions();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], SignalAction::Reply { .. }));
}

#[tokio::test]
async fn router_dispatch_emits_typing_start_and_stop_actions() {
    let mut channel = MockSignalChannel::new();
    let provider: Arc<dyn Provider> = Arc::new(FakeProvider::new("hello"));
    let executor: Arc<dyn ToolExecutor> = Arc::new(DefaultExecutor::new());
    let typing_notifier: Arc<dyn TypingNotifier> =
        Arc::new(SignalTypingNotifier::new(channel.action_sender()));
    let router = build_router(provider, executor, Some(typing_notifier));

    let inbound = inbound_message(
        InboundKind::Text,
        "alice-uuid",
        None,
        false,
        Some("alice-uuid"),
    );
    let (event_tx, _event_rx) = mpsc::channel(64);

    router.dispatch(&inbound, event_tx).await.unwrap();

    let actions = wait_for_actions(&mut channel, 2).await;
    assert!(actions.len() >= 2, "expected at least 2 typing actions");

    assert!(matches!(
        &actions[0],
        SignalAction::Typing {
            target: SignalTarget::Direct(target),
            started: true,
        } if target == "alice-uuid"
    ));

    assert!(matches!(
        &actions[1],
        SignalAction::Typing {
            target: SignalTarget::Direct(target),
            started: false,
        } if target == "alice-uuid"
    ));
}

/// Provider that returns text + tool_request in the first response,
/// then text in the second. Used to test that pre-tool text is flushed
/// separately from post-tool text.
fn scripted_text_before_tool_provider() -> Arc<dyn Provider> {
    Arc::new(ScriptedProvider::new(vec![
        Message::assistant()
            .with_text("before tool")
            .with_tool_request(
                "tool-1",
                "signal_reply",
                serde_json::json!({
                    "chat_id": "alice-uuid",
                    "text": "tool reply",
                    "reply_to_timestamp": 42,
                    "author_id": "alice-uuid"
                }),
            ),
        Message::assistant().with_text("after tool"),
    ]))
}

#[tokio::test]
async fn only_final_text_is_sent_to_channel() {
    let mut channel = MockSignalChannel::new();
    let provider = scripted_text_before_tool_provider();
    let executor: Arc<dyn ToolExecutor> = Arc::new(SignalToolExecutor::new(
        channel.action_sender(),
        dummy_query_tx(),
    ));
    let router = build_router(provider, executor, None);

    channel
        .inject_inbound(inbound_message(
            InboundKind::Text,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    let actions = channel.take_actions();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], SignalAction::Reply { .. }));

    // Only the final assistant text (after all tool use) is delivered.
    // Pre-tool narration is not sent — the user sees one consolidated reply.
    // If the agent needs to notify mid-turn, it uses signal_send explicitly.
    let outbound = channel.take_outbound();
    assert_eq!(
        outbound.len(),
        1,
        "expected 1 outbound message (final reply only), got {}: {:?}",
        outbound.len(),
        outbound.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert_eq!(outbound[0].content, "after tool");
}

fn test_config_verbose() -> Config {
    toml::from_str(
        r#"
[agent]
id = "coop"
model = "test-model"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid"]

[channels.signal]
db_path = "./db/signal.db"
verbose = true
"#,
    )
    .unwrap()
}

fn build_router_verbose(
    provider: Arc<dyn Provider>,
    executor: Arc<dyn ToolExecutor>,
) -> Arc<MessageRouter> {
    let config = test_config_verbose();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("SOUL.md"), "You are a test agent.").unwrap();
    let shared = shared_config(config);
    let gateway = Arc::new(
        Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            ProviderRegistry::new(provider),
            executor,
            None,
            None,
        )
        .unwrap(),
    );

    Arc::new(MessageRouter::new(shared, gateway))
}

#[tokio::test]
async fn verbose_flushes_text_before_each_tool_call() {
    let mut channel = MockSignalChannel::new();
    let provider = scripted_text_before_tool_provider();
    let executor: Arc<dyn ToolExecutor> = Arc::new(SignalToolExecutor::new(
        channel.action_sender(),
        dummy_query_tx(),
    ));
    let router = build_router_verbose(provider, executor);

    channel
        .inject_inbound(inbound_message(
            InboundKind::Text,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    let actions = channel.take_actions();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], SignalAction::Reply { .. }));

    // With verbose=true, pre-tool text is flushed separately before
    // the tool executes, then the post-tool text arrives as a second message.
    let outbound = channel.take_outbound();
    assert_eq!(
        outbound.len(),
        2,
        "expected 2 outbound messages (pre-tool + post-tool), got {}: {:?}",
        outbound.len(),
        outbound.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert_eq!(outbound[0].content, "before tool");
    assert_eq!(outbound[1].content, "after tool");
}

fn build_router_with_gateway(
    provider: Arc<dyn Provider>,
    executor: Arc<dyn ToolExecutor>,
    typing_notifier: Option<Arc<dyn TypingNotifier>>,
) -> (Arc<MessageRouter>, Arc<Gateway>) {
    let config = test_config();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("SOUL.md"), "You are a test agent.").unwrap();
    let shared = shared_config(config);
    let gateway = Arc::new(
        Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            ProviderRegistry::new(provider),
            executor,
            typing_notifier,
            None,
        )
        .unwrap(),
    );

    let router = Arc::new(MessageRouter::new(shared, Arc::clone(&gateway)));
    (router, gateway)
}

fn test_config_two_users() -> Config {
    toml::from_str(
        r#"
[agent]
id = "coop"
model = "test-model"

[[users]]
name = "alice"
trust = "full"
match = ["signal:alice-uuid"]

[[users]]
name = "bob"
trust = "full"
match = ["signal:bob-uuid"]
"#,
    )
    .unwrap()
}

fn build_router_with_typing(
    config: Config,
    provider: Arc<dyn Provider>,
    action_tx: mpsc::Sender<SignalAction>,
) -> (MessageRouter, Arc<Gateway>) {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("SOUL.md"), "You are a test agent.").unwrap();
    let typing: Arc<dyn TypingNotifier> = Arc::new(SignalTypingNotifier::new(action_tx));
    let shared = shared_config(config);
    let gateway = Arc::new(
        Gateway::new(
            Arc::clone(&shared),
            workspace.path().to_path_buf(),
            ProviderRegistry::new(provider),
            Arc::new(DefaultExecutor::new()) as Arc<dyn ToolExecutor>,
            Some(typing),
            None,
        )
        .unwrap(),
    );
    (MessageRouter::new(shared, Arc::clone(&gateway)), gateway)
}

fn has_typing_start(actions: &[SignalAction], uuid: &str) -> bool {
    actions.iter().any(|a| {
        matches!(
            a,
            SignalAction::Typing { target: SignalTarget::Direct(t), started: true } if t == uuid
        )
    })
}

/// Two users messaging on Signal simultaneously should get responses in
/// parallel, not serialized. Uses a slow provider (200ms) and asserts both
/// complete in ~1x the delay, proving concurrency.
#[tokio::test]
async fn concurrent_turns_for_different_sessions_run_in_parallel() {
    use coop_core::fakes::SlowFakeProvider;
    use std::time::{Duration, Instant};

    let delay = Duration::from_millis(200);
    let provider: Arc<dyn Provider> = Arc::new(SlowFakeProvider::new("slow reply", delay));
    let mut channel = MockSignalChannel::new();
    let (router, gateway) =
        build_router_with_typing(test_config_two_users(), provider, channel.action_sender());

    let alice_msg = inbound_message(
        InboundKind::Text,
        "alice-uuid",
        None,
        false,
        Some("alice-uuid"),
    );
    let bob_msg = inbound_message(InboundKind::Text, "bob-uuid", None, false, Some("bob-uuid"));

    // Dispatch both concurrently — this is what run_signal_loop now enables
    // by tracking per-session instead of a single global active_turn.
    let start = Instant::now();
    let alice_task = tokio::spawn({
        let (r, m) = (router.clone(), alice_msg.clone());
        async move { r.dispatch_collect_text(&m).await }
    });
    let bob_task = tokio::spawn({
        let (r, m) = (router.clone(), bob_msg.clone());
        async move { r.dispatch_collect_text(&m).await }
    });

    let (ar, br) = tokio::join!(alice_task, bob_task);
    let elapsed = start.elapsed();
    let (alice_d, alice_text) = ar.unwrap().unwrap();
    let (bob_d, bob_text) = br.unwrap().unwrap();

    // Both complete in ~1x delay, not 2x (proves concurrency).
    assert!(
        elapsed < delay * 2,
        "took {:?}, want <{:?}",
        elapsed,
        delay * 2
    );
    assert_eq!(alice_text, "slow reply");
    assert_eq!(bob_text, "slow reply");

    // Different sessions, each with user + assistant messages
    assert_eq!(
        alice_d.session_key.kind,
        SessionKind::Dm("signal:alice-uuid".into())
    );
    assert_eq!(
        bob_d.session_key.kind,
        SessionKind::Dm("signal:bob-uuid".into())
    );
    assert_eq!(gateway.session_message_count(&alice_d.session_key), 2);
    assert_eq!(gateway.session_message_count(&bob_d.session_key), 2);

    // Both users received independent typing indicators
    let actions = wait_for_actions(&mut channel, 4).await;
    assert!(
        has_typing_start(&actions, "alice-uuid"),
        "alice needs typing start"
    );
    assert!(
        has_typing_start(&actions, "bob-uuid"),
        "bob needs typing start"
    );
}

/// Simulate a crash that left dangling tool_use blocks in the session, then
/// verify that a subsequent Signal message succeeds because the gateway
/// repairs the session before sending to the provider.
#[tokio::test]
async fn signal_e2e_recovers_from_dangling_tool_use() {
    let provider = Arc::new(CountingProvider::new("recovered"));
    let executor = Arc::new(DefaultExecutor::new());
    let (router, gateway) = build_router_with_gateway(
        Arc::clone(&provider) as Arc<dyn Provider>,
        executor as Arc<dyn ToolExecutor>,
        None,
    );

    // Build the session key that will be used for alice-uuid DM
    let session_key = SessionKey {
        agent_id: "coop".to_owned(),
        kind: SessionKind::Dm("signal:alice-uuid".to_owned()),
    };

    // Inject corrupt session state: user msg + assistant with tool_use but no tool_result
    gateway.append_message(&session_key, Message::user().with_text("do something"));
    gateway.append_message(
        &session_key,
        Message::assistant()
            .with_tool_request("tool_a", "bash", serde_json::json!({"command": "echo hi"}))
            .with_tool_request("tool_b", "read_file", serde_json::json!({"path": "x.txt"})),
    );

    // Session has 2 messages, last is assistant with dangling tool_use
    assert_eq!(gateway.messages(&session_key).len(), 2);

    // Now send a new inbound Signal message from alice
    let mut channel = MockSignalChannel::new();
    channel
        .inject_inbound(inbound_message(
            InboundKind::Text,
            "alice-uuid",
            None,
            false,
            Some("alice-uuid"),
        ))
        .await
        .unwrap();

    handle_signal_inbound_once(&mut channel, router.as_ref())
        .await
        .unwrap();

    // The provider should have been called — repair fixed the session
    assert!(provider.calls() > 0, "provider should have been called");

    // Session should have the repair message + user message + assistant response
    let msgs = gateway.messages(&session_key);
    // 2 original + 1 repair (tool_results) + 1 new user + 1 assistant = 5
    assert_eq!(msgs.len(), 5, "session should have 5 messages after repair");

    // Verify the repair message is at index 2 (has tool_results)
    assert!(
        msgs[2].has_tool_results(),
        "repair msg should have tool results"
    );
    // The new user message at index 3
    assert_eq!(msgs[3].role, coop_core::Role::User);
    // The assistant response at index 4
    assert_eq!(msgs[4].role, coop_core::Role::Assistant);

    // Response should have been sent to the channel
    let outbound = channel.take_outbound();
    assert_eq!(outbound.len(), 1);
    assert_eq!(outbound[0].content, "recovered");
}
