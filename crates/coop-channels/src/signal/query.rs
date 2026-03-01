use std::sync::Arc;

use anyhow::{Context, Result};
use coop_core::InboundMessage;
use presage::libsignal_service::prelude::Uuid;
use presage::store::{ContentsStore, Thread};
use tokio::sync::{mpsc, oneshot};
use tracing::{Instrument, debug, info_span, warn};

use super::SignalManager;
use super::SignalTarget;
use super::inbound::parse_content;
use super::name_resolver::SignalNameResolver;

/// A query to the Signal runtime for reading stored messages.
pub enum SignalQuery {
    RecentMessages {
        target: SignalTarget,
        limit: usize,
        before: Option<u64>,
        after: Option<u64>,
        reply: oneshot::Sender<Result<Vec<InboundMessage>>>,
    },
}

// Manual Debug because `oneshot::Sender` prevents derive.
impl std::fmt::Debug for SignalQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecentMessages {
                target,
                limit,
                before,
                after,
                ..
            } => f
                .debug_struct("RecentMessages")
                .field("target", target)
                .field("limit", limit)
                .field("before", before)
                .field("after", after)
                .finish_non_exhaustive(),
        }
    }
}

/// Handles incoming queries on the signal runtime thread.
pub(super) async fn query_task(
    manager: SignalManager,
    mut query_rx: mpsc::Receiver<SignalQuery>,
    resolver: Arc<SignalNameResolver>,
) {
    while let Some(query) = query_rx.recv().await {
        match query {
            SignalQuery::RecentMessages {
                target,
                limit,
                before,
                after,
                reply,
            } => {
                let span = info_span!(
                    "signal_history_query",
                    signal.limit = limit,
                    signal.before = ?before,
                    signal.after = ?after,
                );
                let result = async {
                    fetch_recent_messages(&manager, &target, limit, before, after, &resolver).await
                }
                .instrument(span)
                .await;
                let _ = reply.send(result);
            }
        }
    }
}

async fn fetch_recent_messages(
    manager: &SignalManager,
    target: &SignalTarget,
    limit: usize,
    before: Option<u64>,
    after: Option<u64>,
    resolver: &SignalNameResolver,
) -> Result<Vec<InboundMessage>> {
    let thread = signal_target_to_thread(target)?;
    let start = after.unwrap_or(0);
    let end = before.unwrap_or(u64::MAX);

    let messages_iter = manager
        .store()
        .messages(&thread, start..end)
        .await
        .map_err(|e| anyhow::anyhow!("failed to query signal messages: {e}"))?;

    let mut results = Vec::new();
    for content_result in messages_iter {
        match content_result {
            Ok(content) => {
                if let Some(inbound) = parse_content(&content, Some(resolver)) {
                    results.push(inbound);
                }
            }
            Err(e) => {
                warn!(error = %e, "skipping unreadable message in history");
            }
        }
    }

    // Take last N messages (most recent)
    if results.len() > limit {
        results = results.split_off(results.len() - limit);
    }

    debug!(count = results.len(), "fetched signal history messages");
    Ok(results)
}

fn signal_target_to_thread(target: &SignalTarget) -> Result<Thread> {
    match target {
        SignalTarget::Direct(uuid_str) => {
            let uuid =
                Uuid::parse_str(uuid_str).with_context(|| format!("invalid uuid: {uuid_str}"))?;
            Ok(Thread::Contact(uuid))
        }
        SignalTarget::Group { master_key } => {
            let key: [u8; 32] = master_key
                .as_slice()
                .try_into()
                .map_err(|_slice_err| anyhow::anyhow!("group master key must be 32 bytes"))?;
            Ok(Thread::Group(key))
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_to_thread_direct() {
        let target = SignalTarget::Direct("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned());
        let thread = signal_target_to_thread(&target).unwrap();
        assert!(matches!(thread, Thread::Contact(_)));
    }

    #[test]
    fn target_to_thread_group() {
        let target = SignalTarget::Group {
            master_key: vec![0x11; 32],
        };
        let thread = signal_target_to_thread(&target).unwrap();
        assert!(matches!(thread, Thread::Group(_)));
    }

    #[test]
    fn target_to_thread_rejects_invalid_uuid() {
        let target = SignalTarget::Direct("not-a-uuid".to_owned());
        assert!(signal_target_to_thread(&target).is_err());
    }

    #[test]
    fn target_to_thread_rejects_wrong_key_length() {
        let target = SignalTarget::Group {
            master_key: vec![0x11; 16],
        };
        assert!(signal_target_to_thread(&target).is_err());
    }
}
