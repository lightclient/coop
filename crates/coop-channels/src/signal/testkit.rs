use anyhow::{Result, anyhow};
use async_trait::async_trait;
use coop_core::{Channel, ChannelHealth, InboundMessage, OutboundMessage};
use std::sync::Mutex;
use tokio::sync::mpsc;

use super::SignalAction;

#[derive(Debug)]
pub struct MockSignalChannel {
    id: String,
    inbound_tx: mpsc::Sender<InboundMessage>,
    inbound_rx: mpsc::Receiver<InboundMessage>,
    action_tx: mpsc::Sender<SignalAction>,
    action_rx: mpsc::Receiver<SignalAction>,
    outbound: Mutex<Vec<OutboundMessage>>,
    health: Mutex<ChannelHealth>,
}

impl MockSignalChannel {
    pub fn new() -> Self {
        Self::with_buffer(64)
    }

    pub fn with_buffer(buffer: usize) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(buffer);
        let (action_tx, action_rx) = mpsc::channel(buffer);

        Self {
            id: "signal".to_string(),
            inbound_tx,
            inbound_rx,
            action_tx,
            action_rx,
            outbound: Mutex::new(Vec::new()),
            health: Mutex::new(ChannelHealth::Healthy),
        }
    }

    pub async fn inject_inbound(&self, inbound: InboundMessage) -> Result<()> {
        self.inbound_tx
            .send(inbound)
            .await
            .map_err(|_| anyhow!("signal inbound channel closed"))
    }

    pub fn action_sender(&self) -> mpsc::Sender<SignalAction> {
        self.action_tx.clone()
    }

    pub fn take_outbound(&self) -> Vec<OutboundMessage> {
        std::mem::take(&mut *self.outbound.lock().unwrap())
    }

    pub fn take_actions(&mut self) -> Vec<SignalAction> {
        let mut actions = Vec::new();

        while let Ok(action) = self.action_rx.try_recv() {
            actions.push(action);
        }

        actions
    }

    pub fn set_health(&self, health: ChannelHealth) {
        *self.health.lock().unwrap() = health;
    }
}

impl Default for MockSignalChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for MockSignalChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        self.inbound_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("signal channel closed"))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.outbound.lock().unwrap().push(msg);
        Ok(())
    }

    async fn probe(&self) -> ChannelHealth {
        self.health.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn inbound_message() -> InboundMessage {
        InboundMessage {
            channel: "signal".to_string(),
            sender: "alice-uuid".to_string(),
            content: "hello".to_string(),
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: Some("alice-uuid".to_string()),
            kind: coop_core::InboundKind::Text,
            message_timestamp: Some(1234),
        }
    }

    #[tokio::test]
    async fn inject_and_recv_round_trip() {
        let mut channel = MockSignalChannel::new();
        channel.inject_inbound(inbound_message()).await.unwrap();

        let received = channel.recv().await.unwrap();
        assert_eq!(received.sender, "alice-uuid");
        assert_eq!(received.content, "hello");
    }

    #[tokio::test]
    async fn send_records_outbound() {
        let channel = MockSignalChannel::new();
        channel
            .send(OutboundMessage {
                channel: "signal".to_string(),
                target: "alice-uuid".to_string(),
                content: "reply".to_string(),
            })
            .await
            .unwrap();

        let outbound = channel.take_outbound();
        assert_eq!(outbound.len(), 1);
        assert_eq!(outbound[0].target, "alice-uuid");
        assert_eq!(outbound[0].content, "reply");
    }

    #[tokio::test]
    async fn action_sender_records_actions() {
        let mut channel = MockSignalChannel::new();
        channel
            .action_sender()
            .send(SignalAction::Typing {
                target: super::super::SignalTarget::Direct("alice-uuid".to_string()),
                started: true,
            })
            .await
            .unwrap();

        let actions = channel.take_actions();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            SignalAction::Typing {
                target: super::super::SignalTarget::Direct(target),
                started: true
            } if target == "alice-uuid"
        ));
    }
}
