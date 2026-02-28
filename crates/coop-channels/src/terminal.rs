use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use coop_core::{Channel, ChannelHealth, InboundKind, InboundMessage, OutboundMessage};
use tokio::sync::mpsc;

/// Terminal channel that bridges between a TUI and the gateway via mpsc channels.
#[allow(missing_debug_implementations)] // contains mpsc channels
pub struct TerminalChannel {
    id: String,
    sender: String,
    rx: mpsc::Receiver<String>,
    tx: mpsc::Sender<String>,
}

/// Handle for the TUI side to send/receive messages.
#[allow(missing_debug_implementations)] // contains mpsc channels
pub struct TerminalHandle {
    pub tx: mpsc::Sender<String>,
    pub rx: mpsc::Receiver<String>,
}

/// Create a linked pair of (TerminalChannel, TerminalHandle).
/// Messages sent by the TUI via `TerminalHandle.tx` are received by `TerminalChannel.recv()`.
/// Messages sent by the gateway via `TerminalChannel.send()` are received by `TerminalHandle.rx`.
pub fn terminal_pair(
    buffer: usize,
    sender: impl Into<String>,
) -> (TerminalChannel, TerminalHandle) {
    let (tui_to_gw_tx, tui_to_gw_rx) = mpsc::channel(buffer);
    let (gw_to_tui_tx, gw_to_tui_rx) = mpsc::channel(buffer);

    let channel = TerminalChannel {
        id: "terminal:default".to_owned(),
        sender: sender.into(),
        rx: tui_to_gw_rx,
        tx: gw_to_tui_tx,
    };

    let handle = TerminalHandle {
        tx: tui_to_gw_tx,
        rx: gw_to_tui_rx,
    };

    (channel, handle)
}

#[async_trait]
impl Channel for TerminalChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn recv(&mut self) -> Result<InboundMessage> {
        let content = self
            .rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("terminal channel closed"))?;

        Ok(InboundMessage {
            channel: self.id.clone(),
            sender: self.sender.clone(),
            content,
            chat_id: None,
            is_group: false,
            timestamp: Utc::now(),
            reply_to: None,
            kind: InboundKind::Text,
            message_timestamp: None,
            group_revision: None,
        })
    }

    async fn send(&self, msg: OutboundMessage) -> Result<()> {
        self.tx
            .send(msg.content)
            .await
            .map_err(|_send_err| anyhow::anyhow!("terminal channel receiver dropped"))?;
        Ok(())
    }

    async fn probe(&self) -> ChannelHealth {
        if self.tx.is_closed() {
            ChannelHealth::Unhealthy("channel closed".to_owned())
        } else {
            ChannelHealth::Healthy
        }
    }
}
