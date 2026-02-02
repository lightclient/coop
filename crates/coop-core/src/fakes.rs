use crate::types::*;
use crate::traits::*;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Fake channel for testing.
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

/// Fake agent runtime that returns canned responses.
pub struct FakeRuntime {
    pub response: String,
}

impl FakeRuntime {
    pub fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
        }
    }
}

#[async_trait]
impl AgentRuntime for FakeRuntime {
    async fn turn(
        &self,
        _messages: &[Message],
        _system_prompt: &str,
    ) -> Result<AgentResponse> {
        Ok(AgentResponse::text(&self.response))
    }
}

/// In-memory session store for testing.
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
        let mut store = self.store.lock().unwrap();
        store.entry(key.clone()).or_default().extend(messages.iter().cloned());
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SessionKey>> {
        let store = self.store.lock().unwrap();
        Ok(store.keys().cloned().collect())
    }

    async fn delete(&self, key: &SessionKey) -> Result<()> {
        let mut store = self.store.lock().unwrap();
        store.remove(key);
        Ok(())
    }
}
