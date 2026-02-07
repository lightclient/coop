use anyhow::Result;
use coop_channels::SignalChannel;
use coop_core::{Channel, InboundKind, InboundMessage, OutboundMessage};
use std::sync::Arc;

use crate::router::MessageRouter;

pub(crate) async fn run_signal_loop(
    mut signal_channel: SignalChannel,
    router: Arc<MessageRouter>,
) -> Result<()> {
    loop {
        handle_signal_inbound_once(&mut signal_channel, router.as_ref()).await?;
    }
}

pub(crate) async fn handle_signal_inbound_once<C: Channel>(
    signal_channel: &mut C,
    router: &MessageRouter,
) -> Result<()> {
    let inbound = Channel::recv(signal_channel).await?;
    if !should_dispatch_signal_message(&inbound) {
        trace_signal_inbound("signal inbound filtered", &inbound);
        return Ok(());
    }

    let Some(target) = signal_reply_target(&inbound) else {
        return Ok(());
    };

    trace_signal_inbound("signal inbound dispatched", &inbound);

    let (_decision, response) = router.dispatch_collect_text(&inbound).await?;
    if response.trim().is_empty() {
        return Ok(());
    }

    Channel::send(
        signal_channel,
        OutboundMessage {
            channel: "signal".to_string(),
            target,
            content: response,
        },
    )
    .await
}

fn should_dispatch_signal_message(inbound: &InboundMessage) -> bool {
    !matches!(inbound.kind, InboundKind::Typing | InboundKind::Receipt)
}

fn trace_signal_inbound(message: &'static str, inbound: &InboundMessage) {
    tracing::info!(
        signal.inbound_kind = signal_inbound_kind_name(&inbound.kind),
        signal.sender = %inbound.sender,
        signal.chat_id = ?inbound.chat_id,
        signal.message_timestamp = ?inbound.message_timestamp,
        signal.raw_content = %inbound.content,
        "{message}"
    );
}

fn signal_inbound_kind_name(kind: &InboundKind) -> &'static str {
    match kind {
        InboundKind::Text => "text",
        InboundKind::Reaction => "reaction",
        InboundKind::Typing => "typing",
        InboundKind::Receipt => "receipt",
        InboundKind::Edit => "edit",
        InboundKind::Attachment => "attachment",
    }
}

fn signal_reply_target(msg: &InboundMessage) -> Option<String> {
    if let Some(reply_to) = &msg.reply_to {
        return Some(reply_to.clone());
    }

    if msg.is_group {
        return msg.chat_id.as_ref().map(|chat_id| {
            if chat_id.starts_with("group:") {
                chat_id.clone()
            } else {
                format!("group:{chat_id}")
            }
        });
    }

    Some(msg.sender.clone())
}

#[cfg(test)]
mod tests;
