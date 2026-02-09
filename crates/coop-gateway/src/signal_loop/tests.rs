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
    InboundMessage, Message, ModelInfo, Provider, ProviderStream, ToolDef, ToolExecutor, TurnEvent,
    TypingNotifier, Usage,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::{Config, shared_config};
use crate::gateway::Gateway;

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

    fn model_info(&self) -> &ModelInfo {
        &self.model
    }

    async fn complete(
        &self,
        _system: &str,
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
        _system: &str,
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

    fn model_info(&self) -> &ModelInfo {
        &self.model
    }

    async fn complete(
        &self,
        _system: &str,
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
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        anyhow::bail!("streaming not supported")
    }
}

fn test_config() -> Config {
    serde_yaml::from_str(
        "
agent:
  id: coop
  model: test-model
users:
  - name: alice
    trust: full
    match: ['signal:alice-uuid']
",
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
            provider,
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
async fn text_before_tool_call_is_flushed_separately() {
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

    // Pre-tool text and post-tool text must arrive as separate messages.
    // Before this fix, they were concatenated and sent as one message
    // *after* the tool had already sent its reply — arriving out of order.
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
