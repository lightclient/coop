use anyhow::{Context, Result};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalTarget {
    Direct(String),
    Group { master_key: Vec<u8> },
}

impl SignalTarget {
    pub fn parse(value: &str) -> Result<Self> {
        if let Some(group_hex) = value.strip_prefix("group:") {
            let master_key = hex::decode(group_hex)
                .with_context(|| format!("invalid group target key: {group_hex}"))?;

            anyhow::ensure!(!master_key.is_empty(), "group target key cannot be empty");

            return Ok(Self::Group { master_key });
        }

        anyhow::ensure!(!value.trim().is_empty(), "direct target cannot be empty");
        Ok(Self::Direct(value.to_string()))
    }
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
        let _ = SignalTarget::parse(&msg.target)?;

        self.outbound_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("signal outbound channel closed"))
    }

    async fn probe(&self) -> ChannelHealth {
        self.health.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_target() {
        let target = SignalTarget::parse("alice-uuid").unwrap();
        assert_eq!(target, SignalTarget::Direct("alice-uuid".to_string()));
    }

    #[test]
    fn parse_group_target() {
        let target = SignalTarget::parse("group:deadbeef").unwrap();
        assert_eq!(
            target,
            SignalTarget::Group {
                master_key: vec![0xde, 0xad, 0xbe, 0xef],
            }
        );
    }

    #[test]
    fn reject_invalid_group_key() {
        assert!(SignalTarget::parse("group:not-hex").is_err());
    }
}
