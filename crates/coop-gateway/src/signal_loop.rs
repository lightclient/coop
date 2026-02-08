use anyhow::Result;
use coop_channels::{SignalChannel, SignalTarget};
use coop_core::{Channel, InboundKind, InboundMessage, OutboundMessage, TurnEvent};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

use crate::router::MessageRouter;

/// Max messages to load from Signal history when bootstrapping a new session.
const HISTORY_BOOTSTRAP_LIMIT: usize = 20;

pub(crate) async fn run_signal_loop(
    mut signal_channel: SignalChannel,
    router: Arc<MessageRouter>,
) -> Result<()> {
    loop {
        let inbound = Channel::recv(&mut signal_channel).await?;
        if !should_dispatch_signal_message(&inbound) {
            trace_signal_inbound("signal inbound filtered", &inbound);
            continue;
        }

        let Some(target) = signal_reply_target(&inbound) else {
            continue;
        };

        trace_signal_inbound("signal inbound dispatched", &inbound);

        // Bootstrap: seed session with recent Signal history if this is a new session
        let decision = router.route(&inbound);
        if router.session_is_empty(&decision.session_key)
            && let Ok(signal_target) = SignalTarget::parse(&target)
        {
            match signal_channel
                .query_messages(&signal_target, HISTORY_BOOTSTRAP_LIMIT, None, None)
                .await
            {
                Ok(history) if !history.is_empty() => {
                    router.seed_signal_history(&decision.session_key, &history);
                }
                Err(e) => {
                    warn!(error = %e, "failed to load signal history for bootstrap");
                }
                _ => {}
            }
        }

        dispatch_signal_turn(&mut signal_channel, router.as_ref(), &inbound, &target).await?;
    }
}

/// Handle a single inbound Signal message (without history bootstrap).
/// Used by tests; the production path goes through `run_signal_loop`.
#[cfg(test)]
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
    dispatch_signal_turn(signal_channel, router, &inbound, &target).await
}

/// Dispatch a single inbound message: run the agent turn and stream
/// text/tool events to the channel.
async fn dispatch_signal_turn<C: Channel>(
    signal_channel: &mut C,
    router: &MessageRouter,
    inbound: &InboundMessage,
    target: &str,
) -> Result<()> {
    // Process turn events inline so that text produced before a tool call
    // is flushed to the channel *before* the tool executes. This prevents
    // tool side-effects (e.g. signal_reply) from arriving before the
    // preceding text.
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let router = router.clone();
    let message = inbound.clone();
    let dispatch_task = tokio::spawn(async move { router.dispatch(&message, event_tx).await });

    let mut text = String::new();

    while let Some(event) = event_rx.recv().await {
        match event {
            TurnEvent::TextDelta(delta) => {
                text.push_str(&delta);
            }
            TurnEvent::ToolStart { .. } => {
                flush_text(signal_channel, target, &mut text).await?;
            }
            TurnEvent::Error(message) => {
                text = message;
            }
            TurnEvent::Done(_) => {
                break;
            }
            TurnEvent::AssistantMessage(_) | TurnEvent::ToolResult { .. } => {}
        }
    }

    flush_text(signal_channel, target, &mut text).await?;

    match dispatch_task.await {
        Ok(result) => result.map(|_| ()),
        Err(error) => anyhow::bail!("router task failed: {error}"),
    }
}

/// Send accumulated text to the channel and clear the buffer.
async fn flush_text<C: Channel>(channel: &C, target: &str, text: &mut String) -> Result<()> {
    if text.trim().is_empty() {
        text.clear();
    } else {
        let content = std::mem::take(text);
        Channel::send(
            channel,
            OutboundMessage {
                channel: "signal".to_owned(),
                target: target.to_owned(),
                content,
            },
        )
        .await?;
    }
    Ok(())
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
