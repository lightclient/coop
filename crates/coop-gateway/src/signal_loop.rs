use anyhow::Result;
use coop_channels::{SignalChannel, SignalTarget};
use coop_core::{Channel, InboundKind, InboundMessage, OutboundMessage, SessionKey, TurnEvent};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::router::MessageRouter;

/// Max messages to load from Signal history when bootstrapping a new session.
const HISTORY_BOOTSTRAP_LIMIT: usize = 20;

pub(crate) async fn run_signal_loop(
    mut signal_channel: SignalChannel,
    router: Arc<MessageRouter>,
) -> Result<()> {
    tracing::info!("signal loop listening");

    // Per-session turn tracking: each session can have one active turn.
    // Different sessions run concurrently.
    let mut active_turns: HashMap<SessionKey, JoinHandle<Result<()>>> = HashMap::new();

    loop {
        let inbound = Channel::recv(&mut signal_channel).await?;
        if !should_dispatch_signal_message(&inbound) {
            trace_signal_inbound("signal inbound filtered", &inbound);
            continue;
        }

        let Some(target) = signal_reply_target(&inbound) else {
            continue;
        };

        // Commands are handled immediately, even while a turn is running.
        // The router dispatches commands synchronously without blocking on
        // the agent loop.
        if inbound.kind == InboundKind::Command {
            trace_signal_inbound("signal command dispatched", &inbound);
            dispatch_command(&signal_channel, &router, &inbound, &target).await?;
            continue;
        }

        trace_signal_inbound("signal inbound dispatched", &inbound);

        // Clean up completed turns (finished or panicked)
        active_turns.retain(|_, task| !task.is_finished());

        // Route to determine the target session
        let decision = router.route(&inbound);

        // If this session already has an active turn, skip.
        // The gateway's try_lock would reject it anyway, but we avoid the
        // spawn overhead and log a clearer message.
        if active_turns.contains_key(&decision.session_key) {
            tracing::debug!(
                session = %decision.session_key,
                "skipping message: session already has an active turn"
            );
            continue;
        }

        // Bootstrap: seed session with recent Signal history if this is a new session
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

        // Spawn the turn as a background task so the main loop stays free
        // to process commands (e.g. /stop, /status).
        let router_clone = Arc::clone(&router);
        let inbound_clone = inbound.clone();
        let target_clone = target.clone();
        let action_tx = signal_channel.action_sender();
        let session_key = decision.session_key.clone();
        active_turns.insert(
            session_key,
            tokio::spawn(async move {
                dispatch_signal_turn_background(
                    &action_tx,
                    router_clone.as_ref(),
                    &inbound_clone,
                    &target_clone,
                )
                .await
            }),
        );
    }
}

/// Dispatch a command immediately, sending the response back to Signal.
async fn dispatch_command(
    signal_channel: &SignalChannel,
    router: &MessageRouter,
    inbound: &InboundMessage,
    target: &str,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let router = router.clone();
    let message = inbound.clone();

    // Commands are handled synchronously in dispatch() — they don't
    // go through the agent turn loop, so this completes immediately.
    let dispatch_task = tokio::spawn(async move { router.dispatch(&message, event_tx).await });

    let mut text = String::new();
    while let Some(event) = event_rx.recv().await {
        match event {
            TurnEvent::TextDelta(delta) => text.push_str(&delta),
            TurnEvent::Done(_) => break,
            _ => {}
        }
    }

    flush_text(signal_channel, target, &mut text).await?;

    match dispatch_task.await {
        Ok(result) => result.map(|_| ()),
        Err(error) => anyhow::bail!("router task failed: {error}"),
    }
}

/// Dispatch a turn in the background, sending responses via the action_tx
/// channel instead of requiring `&mut SignalChannel`.
///
/// NOTE: The verbose/quiet event-collection logic here is Signal-specific but
/// generic enough for any messaging channel. When a second chat channel is
/// added (WhatsApp, Telegram, etc.), extract this into a shared
/// `router.dispatch_to_channel(msg, verbose, send_fn)` method — see
/// `router::dispatch_collect_text` for the cron precedent.
async fn dispatch_signal_turn_background(
    action_tx: &mpsc::Sender<coop_channels::SignalAction>,
    router: &MessageRouter,
    inbound: &InboundMessage,
    target: &str,
) -> Result<()> {
    let verbose = router.signal_verbose();
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
                if verbose {
                    flush_text_via_action(action_tx, target, &mut text).await?;
                }
            }
            TurnEvent::ToolResult { .. } | TurnEvent::Compacting => {}
            TurnEvent::AssistantMessage(ref msg) => {
                if !verbose {
                    // In quiet mode, only keep the text from the *last*
                    // assistant message (the final reply after all tool use).
                    let msg_text = msg.text();
                    if !msg.has_tool_requests() && !msg_text.is_empty() {
                        text = msg_text;
                    }
                }
            }
            TurnEvent::Error(message) => {
                text = message;
            }
            TurnEvent::Done(_) => {
                break;
            }
        }
    }

    flush_text_via_action(action_tx, target, &mut text).await?;

    match dispatch_task.await {
        Ok(result) => result.map(|_| ()),
        Err(error) => {
            // The dispatch task panicked or was cancelled. The user never
            // received a response because the event channel was dropped
            // before any text was produced. Send a fallback error message
            // so the conversation doesn't silently hang.
            tracing::error!(
                error = %error,
                target = target,
                "dispatch task failed, sending fallback error to user"
            );
            let _ = flush_text_via_action(
                action_tx,
                target,
                &mut "Something went wrong processing that message. Please try again.".to_owned(),
            )
            .await;
            anyhow::bail!("router task failed: {error}");
        }
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
    let verbose = router.signal_verbose();
    dispatch_signal_turn(signal_channel, router, &inbound, &target, verbose).await
}

/// Dispatch a single inbound message: mirrors production
/// `dispatch_signal_turn_background` behavior. When `verbose` is false,
/// only the final assistant text is sent. When true, text is flushed
/// before each tool call.
#[cfg(test)]
async fn dispatch_signal_turn<C: Channel>(
    signal_channel: &mut C,
    router: &MessageRouter,
    inbound: &InboundMessage,
    target: &str,
    verbose: bool,
) -> Result<()> {
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
                if verbose {
                    flush_text(signal_channel, target, &mut text).await?;
                }
            }
            TurnEvent::ToolResult { .. } | TurnEvent::Compacting => {}
            TurnEvent::AssistantMessage(ref msg) => {
                if !verbose {
                    let msg_text = msg.text();
                    if !msg.has_tool_requests() && !msg_text.is_empty() {
                        text = msg_text;
                    }
                }
            }
            TurnEvent::Error(message) => {
                text = message;
            }
            TurnEvent::Done(_) => {
                break;
            }
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

/// Send accumulated text via the action channel (for background tasks that
/// don't hold `&mut SignalChannel`).
async fn flush_text_via_action(
    action_tx: &mpsc::Sender<coop_channels::SignalAction>,
    target: &str,
    text: &mut String,
) -> Result<()> {
    if text.trim().is_empty() {
        text.clear();
    } else {
        let content = std::mem::take(text);
        action_tx
            .send(coop_channels::SignalAction::SendText(OutboundMessage {
                channel: "signal".to_owned(),
                target: target.to_owned(),
                content,
            }))
            .await
            .map_err(|_send_err| anyhow::anyhow!("signal action channel closed"))?;
    }
    Ok(())
}

fn should_dispatch_signal_message(inbound: &InboundMessage) -> bool {
    !matches!(inbound.kind, InboundKind::Typing | InboundKind::Receipt)
}

fn trace_signal_inbound(message: &'static str, inbound: &InboundMessage) {
    tracing::debug!(
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
        InboundKind::Command => "command",
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
