//! Fake implementations for testing.
#![allow(clippy::unwrap_used)]

use crate::traits::{
    Channel, Provider, ProviderStream, SessionStore, Tool, ToolContext, ToolExecutor,
};
use crate::types::{
    ChannelHealth, InboundMessage, Message, ModelInfo, OutboundMessage, SessionKey, ToolDef,
    ToolOutput, Usage,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// FakeChannel
// ---------------------------------------------------------------------------

/// Fake channel for testing.
#[derive(Debug)]
pub struct FakeChannel {
    pub id: String,
    pub inbound: Mutex<Vec<InboundMessage>>,
    pub outbound: Mutex<Vec<OutboundMessage>>,
}

impl FakeChannel {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            inbound: Mutex::new(Vec::new()),
            outbound: Mutex::new(Vec::new()),
        }
    }

    pub fn push_inbound(&self, msg: InboundMessage) {
        self.inbound.lock().unwrap().push(msg);
    }

    pub fn take_outbound(&self) -> Vec<OutboundMessage> {
        std::mem::take(&mut *self.outbound.lock().unwrap())
    }
}

#[async_trait]
impl Channel for FakeChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        let mut queue = self.inbound.lock().unwrap();
        if queue.is_empty() {
            anyhow::bail!("no inbound messages");
        }
        Ok(queue.remove(0))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.outbound.lock().unwrap().push(msg);
        Ok(())
    }

    async fn probe(&self) -> ChannelHealth {
        ChannelHealth::Healthy
    }
}

// ---------------------------------------------------------------------------
// FakeProvider
// ---------------------------------------------------------------------------

/// Fake provider that returns canned responses.
#[derive(Debug)]
pub struct FakeProvider {
    pub name: String,
    pub model: ModelInfo,
    /// The text response to return on each complete() call.
    pub response: Mutex<String>,
}

impl FakeProvider {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            name: "fake".into(),
            model: ModelInfo {
                name: "fake-model".into(),
                context_limit: 128_000,
            },
            response: Mutex::new(response.into()),
        }
    }

    pub fn set_response(&self, response: impl Into<String>) {
        *self.response.lock().unwrap() = response.into();
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        &self.name
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
        let text = self.response.lock().unwrap().clone();
        Ok((
            Message::assistant().with_text(text),
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(50),
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
        anyhow::bail!("FakeProvider does not support streaming")
    }
}

// ---------------------------------------------------------------------------
// SlowFakeProvider
// ---------------------------------------------------------------------------

/// Fake provider that sleeps before returning, simulating slow API calls.
#[derive(Debug)]
pub struct SlowFakeProvider {
    inner: FakeProvider,
    delay: std::time::Duration,
}

impl SlowFakeProvider {
    pub fn new(response: impl Into<String>, delay: std::time::Duration) -> Self {
        Self {
            inner: FakeProvider::new(response),
            delay,
        }
    }
}

#[async_trait]
impl Provider for SlowFakeProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model_info(&self) -> &ModelInfo {
        self.inner.model_info()
    }

    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<(Message, Usage)> {
        tokio::time::sleep(self.delay).await;
        self.inner.complete(system, messages, tools).await
    }

    async fn stream(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<ProviderStream> {
        anyhow::bail!("SlowFakeProvider does not support streaming")
    }
}

// ---------------------------------------------------------------------------
// FakeTool
// ---------------------------------------------------------------------------

/// Fake tool for testing.
#[derive(Debug)]
pub struct FakeTool {
    pub def: ToolDef,
    pub output: Mutex<ToolOutput>,
}

impl FakeTool {
    pub fn new(name: impl Into<String>, output: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            def: ToolDef::new(name, "A fake tool", serde_json::json!({"type": "object"})),
            output: Mutex::new(ToolOutput::success(output)),
        }
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn definition(&self) -> ToolDef {
        self.def.clone()
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        Ok(self.output.lock().unwrap().clone())
    }
}

// ---------------------------------------------------------------------------
// SimpleExecutor
// ---------------------------------------------------------------------------

/// Simple tool executor backed by a list of tools.
#[allow(missing_debug_implementations)]
pub struct SimpleExecutor {
    tools: Vec<Box<dyn Tool>>,
}

impl SimpleExecutor {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn add(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }
}

impl Default for SimpleExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for SimpleExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        for tool in &self.tools {
            if tool.definition().name == name {
                return tool.execute(arguments, ctx).await;
            }
        }
        anyhow::bail!("unknown tool: {name}")
    }

    fn tools(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|t| t.definition()).collect()
    }
}

// ---------------------------------------------------------------------------
// MemorySessionStore
// ---------------------------------------------------------------------------

/// In-memory session store for testing.
#[derive(Debug)]
pub struct MemorySessionStore {
    store: Mutex<HashMap<SessionKey, Vec<Message>>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn load(&self, key: &SessionKey) -> Result<Vec<Message>> {
        let store = self.store.lock().unwrap();
        Ok(store.get(key).cloned().unwrap_or_default())
    }

    async fn save(&self, key: &SessionKey, messages: &[Message]) -> Result<()> {
        self.store
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .extend(messages.iter().cloned());
        Ok(())
    }

    async fn replace(&self, key: &SessionKey, messages: &[Message]) -> Result<()> {
        self.store
            .lock()
            .unwrap()
            .insert(key.clone(), messages.to_vec());
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SessionKey>> {
        Ok(self.store.lock().unwrap().keys().cloned().collect())
    }

    async fn delete(&self, key: &SessionKey) -> Result<()> {
        self.store.lock().unwrap().remove(key);
        Ok(())
    }
}
