use anyhow::Result;
use async_trait::async_trait;
use coop_core::{Channel, ChannelHealth, InboundMessage, OutboundMessage};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[allow(missing_debug_implementations)]
pub struct SignalChannel {
    id: String,
    inbound_rx: mpsc::Receiver<InboundMessage>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
    health: Arc<Mutex<ChannelHealth>>,
}

#[allow(missing_debug_implementations)]
pub struct SignalHandle {
    pub inbound_tx: mpsc::Sender<InboundMessage>,
    pub outbound_rx: mpsc::Receiver<OutboundMessage>,
    health: Arc<Mutex<ChannelHealth>>,
}

pub fn signal_pair(buffer: usize) -> (SignalChannel, SignalHandle) {
    let (inbound_tx, inbound_rx) = mpsc::channel(buffer);
    let (outbound_tx, outbound_rx) = mpsc::channel(buffer);
    let health = Arc::new(Mutex::new(ChannelHealth::Healthy));

    (
        SignalChannel {
            id: "signal".to_string(),
            inbound_rx,
            outbound_tx,
            health: health.clone(),
        },
        SignalHandle {
            inbound_tx,
            outbound_rx,
            health,
        },
    )
}

impl SignalHandle {
    pub fn set_health(&self, health: ChannelHealth) {
        *self.health.lock().unwrap() = health;
    }
}

#[async_trait]
impl Channel for SignalChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        self.inbound_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("signal channel closed"))
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.outbound_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("signal outbound channel closed"))
    }

    async fn probe(&self) -> ChannelHealth {
        self.health.lock().unwrap().clone()
    }
}
