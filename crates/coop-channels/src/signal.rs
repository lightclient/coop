#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod image_ext_tests;
mod inbound;
pub(crate) mod name_resolver;
mod query;
#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod target_tests;
pub mod testkit;

pub use query::SignalQuery;
pub use testkit::MockSignalChannel;

use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_core::{
    Channel, ChannelHealth, InboundMessage, OutboundMessage, SessionKey, SessionKind,
    TypingNotifier,
};
use futures::StreamExt;
use inbound::{format_attachment_metadata, inbound_from_content};
use presage::libsignal_service::content::{ContentBody, DataMessage, GroupContextV2};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::libsignal_service::sender::AttachmentSpec;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::proto::data_message::{Quote, Reaction};
use presage::proto::{AttachmentPointer, TypingMessage, typing_message};
use presage::{Manager, store::StateStore};
use presage_store_sqlite::{SqliteConnectOptions, SqliteStore};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, info, info_span, warn};

pub(crate) type SignalManager = Manager<SqliteStore, Registered>;
type HealthState = Arc<Mutex<ChannelHealth>>;

#[derive(Debug, Clone)]
pub enum SignalAction {
    SendText(OutboundMessage),
    SendAttachment {
        target: SignalTarget,
        path: PathBuf,
        mime_type: String,
        caption: Option<String>,
    },
    React {
        target: SignalTarget,
        emoji: String,
        target_author_aci: String,
        target_sent_timestamp: u64,
        remove: bool,
    },
    Reply {
        target: SignalTarget,
        text: String,
        quote_timestamp: u64,
        quote_author_aci: String,
    },
    Typing {
        target: SignalTarget,
        started: bool,
    },
    SendReceipt {
        sender_uuid: String,
        timestamps: Vec<u64>,
        receipt_type: SignalReceiptType,
    },
    Shutdown,
}

#[allow(missing_debug_implementations)]
pub struct SignalChannel {
    id: String,
    inbound_rx: mpsc::Receiver<InboundMessage>,
    action_tx: mpsc::Sender<SignalAction>,
    query_tx: mpsc::Sender<SignalQuery>,
    health: HealthState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalReceiptType {
    Delivery,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalTarget {
    Direct(String),
    Group { master_key: Vec<u8> },
}

impl SignalTarget {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim().trim_start_matches("signal:");

        if let Some(group_hex) = value.strip_prefix("group:") {
            let master_key = hex::decode(group_hex)
                .with_context(|| format!("invalid group target key: {group_hex}"))?;

            anyhow::ensure!(!master_key.is_empty(), "group target key cannot be empty");
            anyhow::ensure!(
                master_key.len() == 32,
                "group target key must be exactly 32 bytes"
            );

            return Ok(Self::Group { master_key });
        }

        anyhow::ensure!(!value.is_empty(), "direct target cannot be empty");
        Ok(Self::Direct(value.to_owned()))
    }
}

#[derive(Debug, Clone)]
pub struct SignalTypingNotifier {
    action_tx: mpsc::Sender<SignalAction>,
}

impl SignalTypingNotifier {
    pub fn new(action_tx: mpsc::Sender<SignalAction>) -> Self {
        Self { action_tx }
    }
}

#[async_trait]
impl TypingNotifier for SignalTypingNotifier {
    async fn set_typing(&self, session_key: &SessionKey, started: bool) {
        let target = match &session_key.kind {
            SessionKind::Dm(identity) => {
                let identity = identity.strip_prefix("signal:").unwrap_or(identity);
                match SignalTarget::parse(identity) {
                    Ok(target) => target,
                    Err(_) => return,
                }
            }
            SessionKind::Group(group_id) => {
                let group_id = group_id.strip_prefix("signal:").unwrap_or(group_id);
                match SignalTarget::parse(group_id) {
                    Ok(target) => target,
                    Err(_) => return,
                }
            }
            SessionKind::Main | SessionKind::Isolated(_) | SessionKind::Cron(_) => return,
        };

        let _ = self
            .action_tx
            .send(SignalAction::Typing { target, started })
            .await;
    }
}

impl SignalChannel {
    /// Connect to Signal.
    ///
    /// `trusted_senders` is the set of Signal sender UUIDs with at least
    /// inner trust. Attachments are only downloaded and saved for senders
    /// in this set.
    ///
    /// `agent_name` is the agent's display name (typically `agent.id`).
    /// `user_mappings` maps Signal ACIs to coop user names from `[[users]]` config.
    pub async fn connect(
        db_path: impl AsRef<Path>,
        attachments_dir: impl Into<PathBuf>,
        trusted_senders: HashSet<String>,
        agent_name: String,
        user_mappings: Vec<(String, String)>,
    ) -> Result<Self> {
        let manager = load_registered_manager(db_path.as_ref()).await?;

        let self_aci = manager.registration_data().service_ids.aci.to_string();
        info!(self_aci = %self_aci, agent_name = %agent_name, "building signal name resolver");

        let contacts = load_contacts(db_path.as_ref()).await;
        let resolver = Arc::new(name_resolver::SignalNameResolver::build(
            self_aci,
            agent_name,
            &user_mappings,
            &contacts,
        ));

        let (inbound_tx, inbound_rx) = mpsc::channel(64);
        let (action_tx, action_rx) = mpsc::channel(64);
        let (query_tx, query_rx) = mpsc::channel(16);
        let health = Arc::new(Mutex::new(ChannelHealth::Healthy));

        start_signal_runtime(
            manager,
            inbound_tx,
            action_tx.clone(),
            action_rx,
            query_rx,
            Arc::clone(&health),
            attachments_dir.into(),
            trusted_senders,
            Arc::clone(&resolver),
        );

        Ok(Self {
            id: "signal".to_owned(),
            inbound_rx,
            action_tx,
            query_tx,
            health,
        })
    }

    pub fn action_sender(&self) -> mpsc::Sender<SignalAction> {
        self.action_tx.clone()
    }

    pub fn query_sender(&self) -> mpsc::Sender<SignalQuery> {
        self.query_tx.clone()
    }

    /// Query recent messages from the Signal store for a given target.
    pub async fn query_messages(
        &self,
        target: &SignalTarget,
        limit: usize,
        before: Option<u64>,
        after: Option<u64>,
    ) -> Result<Vec<InboundMessage>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.query_tx
            .send(SignalQuery::RecentMessages {
                target: target.clone(),
                limit,
                before,
                after,
                reply: reply_tx,
            })
            .await
            .map_err(|_send_err| anyhow::anyhow!("signal query channel closed"))?;

        reply_rx
            .await
            .map_err(|_recv_err| anyhow::anyhow!("signal query response lost"))?
    }

    pub async fn link_device<F>(
        db_path: &Path,
        device_name: String,
        on_provisioning_url: F,
    ) -> Result<()>
    where
        F: FnOnce(&str) -> Result<()>,
    {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let store = open_store(db_path).await?;
        let (provisioning_tx, provisioning_rx) = futures::channel::oneshot::channel();

        let (manager, provisioning_result) = futures::future::join(
            Manager::link_secondary_device(
                store,
                presage::libsignal_service::configuration::SignalServers::Production,
                device_name,
                provisioning_tx,
            ),
            async move {
                let url = provisioning_rx
                    .await
                    .context("failed to receive provisioning url")?;
                on_provisioning_url(url.as_str())
            },
        )
        .await;

        let manager = manager.context("failed to complete signal linking")?;
        provisioning_result?;

        tracing::info!(
            service_ids = %manager.registration_data().service_ids,
            "signal linking completed"
        );

        Ok(())
    }

    pub async fn unlink(db_path: &Path) -> Result<()> {
        if !db_path.exists() {
            return Ok(());
        }

        let mut store = open_store(db_path).await?;
        store
            .clear_registration()
            .await
            .context("failed to clear signal registration")?;

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn start_signal_runtime(
    manager: SignalManager,
    inbound_tx: mpsc::Sender<InboundMessage>,
    action_tx: mpsc::Sender<SignalAction>,
    action_rx: mpsc::Receiver<SignalAction>,
    query_rx: mpsc::Receiver<SignalQuery>,
    health: HealthState,
    attachments_dir: PathBuf,
    trusted_senders: HashSet<String>,
    resolver: Arc<name_resolver::SignalNameResolver>,
) {
    std::thread::Builder::new()
        .name("signal-runtime".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    set_health(
                        &health,
                        ChannelHealth::Unhealthy(format!(
                            "failed to start signal runtime: {error}"
                        )),
                    );
                    return;
                }
            };

            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                info!("signal runtime started");

                let receive_manager = manager.clone();
                let query_manager = manager.clone();
                let receive_health = Arc::clone(&health);
                let send_health = Arc::clone(&health);
                let receive_resolver = Arc::clone(&resolver);
                let query_resolver = Arc::clone(&resolver);

                let receive_task = tokio::task::spawn_local(Box::pin(receive_task(
                    receive_manager,
                    inbound_tx,
                    action_tx,
                    receive_health,
                    attachments_dir,
                    trusted_senders,
                    receive_resolver,
                )));
                let send_task =
                    tokio::task::spawn_local(Box::pin(send_task(manager, action_rx, send_health)));
                let query_task = tokio::task::spawn_local(Box::pin(query::query_task(
                    query_manager,
                    query_rx,
                    query_resolver,
                )));

                let _ = futures::future::join3(receive_task, send_task, query_task).await;
            });
        })
        .expect("failed to spawn signal runtime thread");
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
        self.action_tx
            .send(SignalAction::SendText(msg))
            .await
            .map_err(|_send_err| anyhow::anyhow!("signal action channel closed"))
    }

    async fn probe(&self) -> ChannelHealth {
        self.health.lock().expect("health mutex poisoned").clone()
    }
}

#[allow(clippy::large_futures, clippy::too_many_arguments)]
async fn receive_task(
    mut manager: SignalManager,
    inbound_tx: mpsc::Sender<InboundMessage>,
    action_tx: mpsc::Sender<SignalAction>,
    health: HealthState,
    attachments_dir: PathBuf,
    trusted_senders: HashSet<String>,
    resolver: Arc<name_resolver::SignalNameResolver>,
) {
    // Separate manager clone for downloading attachments. The receive stream
    // holds `&mut manager`, so we can't call `get_attachment` on the same
    // instance while iterating.
    let attachment_manager = manager.clone();
    let mut backoff = Duration::from_secs(1);

    loop {
        match Box::pin(manager.receive_messages()).await {
            Ok(messages) => {
                set_health(&health, ChannelHealth::Healthy);
                info!("signal websocket connected, receiving messages");
                backoff = Duration::from_secs(1);

                tokio::pin!(messages);
                while let Some(received) = messages.next().await {
                    let Received::Content(content) = received else {
                        continue;
                    };

                    let sender = content.metadata.sender.raw_uuid().to_string();
                    let content_body = signal_content_body_name(&content.body);
                    let timestamp = content.metadata.timestamp;
                    let needs_receipt = matches!(
                        &content.body,
                        ContentBody::DataMessage(_) | ContentBody::EditMessage(_)
                    );

                    let receive_span = info_span!(
                        "signal_receive_event",
                        signal.sender = %sender,
                        signal.content_body = content_body,
                        signal.timestamp = timestamp,
                    );

                    let inbound = {
                        let _guard = receive_span.enter();
                        inbound_from_content(&content, Some(&resolver))
                    };

                    if let Some(mut inbound) = inbound {
                        if needs_receipt && inbound.message_timestamp.is_some() {
                            let _ = action_tx
                                .send(SignalAction::SendReceipt {
                                    sender_uuid: sender.clone(),
                                    timestamps: vec![timestamp],
                                    receipt_type: SignalReceiptType::Delivery,
                                })
                                .await;
                            let _ = action_tx
                                .send(SignalAction::SendReceipt {
                                    sender_uuid: sender.clone(),
                                    timestamps: vec![timestamp],
                                    receipt_type: SignalReceiptType::Read,
                                })
                                .await;
                        }

                        // Download and save attachments only from senders
                        // with at least inner trust.
                        let pointers = extract_attachment_pointers(&content);
                        if !pointers.is_empty() && trusted_senders.contains(&sender) {
                            download_and_rewrite_attachments(
                                &attachment_manager,
                                &pointers,
                                &attachments_dir,
                                timestamp,
                                &mut inbound,
                            )
                            .await;
                        }

                        debug!(
                            signal.inbound_kind = ?inbound.kind,
                            signal.sender = %inbound.sender,
                            signal.chat_id = ?inbound.chat_id,
                            signal.message_timestamp = ?inbound.message_timestamp,
                            signal.raw_content = %inbound.content,
                            "received signal inbound"
                        );

                        if inbound_tx.send(inbound).await.is_err() {
                            return;
                        }
                    }
                }

                warn!("signal receive stream ended, will reconnect");
                set_health(
                    &health,
                    ChannelHealth::Degraded("signal receive stream ended".to_owned()),
                );
            }
            Err(error) => {
                warn!(error = %error, "signal receive setup failed");
                set_health(
                    &health,
                    ChannelHealth::Degraded(format!("signal receive failed: {error}")),
                );
            }
        }

        tokio::time::sleep(backoff).await;
        let next_secs = backoff.as_secs().saturating_mul(2).min(30);
        backoff = Duration::from_secs(next_secs.max(1));
    }
}

#[allow(clippy::large_futures)]
async fn send_task(
    mut manager: SignalManager,
    mut action_rx: mpsc::Receiver<SignalAction>,
    health: HealthState,
) {
    while let Some(action) = action_rx.recv().await {
        if matches!(action, SignalAction::Shutdown) {
            info!("signal send task shutting down gracefully");
            break;
        }

        debug!(action = ?action, "sending signal action");

        // Flush stale websockets before sending real messages.
        //
        // Presage multiplexes send and receive over a shared identified
        // websocket with a 55-second keepalive. If the underlying TCP
        // connection dies silently (NAT timeout, ISP maintenance, etc.),
        // up to 55 seconds can pass before the keepalive detects the
        // failure and marks the socket as closed.  During that window
        // `is_closed()` still returns false, so `Manager::send_message`
        // will try to push through the dead connection, fail with a
        // non-retryable `WsClosing` error, and the message is lost.
        //
        // `whoami()` is a lightweight authenticated request that forces
        // presage to use (and therefore test) the identified websocket.
        // If the connection is dead it will fail fast and presage will
        // replace the cached socket on the next call.  The actual
        // `send_signal_action` that follows will then get a fresh
        // connection.
        //
        // We skip the probe for ephemeral actions (typing indicators,
        // receipts) where a dropped message is harmless.
        if is_durable_action(&action)
            && let Err(error) = Box::pin(manager.whoami()).await
        {
            warn!(error = %error, "pre-send websocket probe failed, connection will be refreshed");
        }

        match send_signal_action(&mut manager, action).await {
            Ok(()) => set_health(&health, ChannelHealth::Healthy),
            Err(error) => {
                warn!(error = %error, "failed to send signal action");
                set_health(
                    &health,
                    ChannelHealth::Degraded(format!("signal send failed: {error}")),
                );
            }
        }
    }

    set_health(
        &health,
        ChannelHealth::Unhealthy("signal sender task stopped".to_owned()),
    );
}

fn is_durable_action(action: &SignalAction) -> bool {
    !matches!(
        action,
        SignalAction::Typing { .. } | SignalAction::SendReceipt { .. }
    )
}

#[allow(clippy::large_futures, clippy::too_many_lines)]
async fn send_signal_action(manager: &mut SignalManager, action: SignalAction) -> Result<()> {
    match action {
        SignalAction::SendText(outbound) => {
            let target = SignalTarget::parse(&outbound.target)?;
            let target_kind = signal_target_kind(&target);
            let target_value = signal_target_value(&target);
            let timestamp = now_epoch_millis();

            let (content, redacted) = name_resolver::sanitize_uuids(&outbound.content);
            if redacted > 0 {
                warn!(
                    count = redacted,
                    "redacted UUIDs from outbound signal message"
                );
            }

            let raw_content = content.clone();
            let span = info_span!(
                "signal_action_send",
                signal.action = "send_text",
                signal.target_kind = target_kind,
                signal.target = %target_value,
                signal.timestamp = timestamp,
                signal.raw_content = %raw_content,
            );
            let message = DataMessage {
                body: Some(content),
                group_v2: group_context_for_target(&target),
                ..Default::default()
            };
            send_action_with_trace(manager, span, target, message, timestamp).await
        }
        SignalAction::SendAttachment {
            target,
            path,
            mime_type,
            caption,
        } => {
            let target_kind = signal_target_kind(&target);
            let target_value = signal_target_value(&target);
            let timestamp = now_epoch_millis();
            let path_display = path.display().to_string();
            let mime_for_trace = mime_type.clone();
            let span = info_span!(
                "signal_action_send",
                signal.action = "send_attachment",
                signal.target_kind = target_kind,
                signal.target = %target_value,
                signal.timestamp = timestamp,
                signal.attachment_path = %path_display,
                signal.attachment_mime = %mime_for_trace,
            );

            let file_data = std::fs::read(&path)
                .with_context(|| format!("failed to read attachment: {}", path.display()))?;
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_owned();

            let spec = AttachmentSpec {
                content_type: mime_type,
                length: file_data.len(),
                file_name: Some(file_name),
                preview: None,
                voice_note: None,
                borderless: None,
                width: None,
                height: None,
                caption: caption.clone(),
                blur_hash: None,
            };

            let attachment = manager
                .upload_attachment(spec, file_data)
                .await
                .context("failed to upload signal attachment")?
                .map_err(|e| anyhow::anyhow!("attachment upload error: {e:?}"))?;

            let message = DataMessage {
                body: caption,
                attachments: vec![attachment],
                group_v2: group_context_for_target(&target),
                ..Default::default()
            };
            send_action_with_trace(manager, span, target, message, timestamp).await
        }
        SignalAction::React {
            target,
            emoji,
            target_author_aci,
            target_sent_timestamp,
            remove,
        } => {
            let target_kind = signal_target_kind(&target);
            let target_value = signal_target_value(&target);
            let timestamp = now_epoch_millis();
            let emoji_for_trace = emoji.clone();
            let target_author_aci_for_trace = target_author_aci.clone();
            let span = info_span!(
                "signal_action_send",
                signal.action = "react",
                signal.target_kind = target_kind,
                signal.target = %target_value,
                signal.timestamp = timestamp,
                signal.emoji = %emoji_for_trace,
                signal.remove = remove,
                signal.target_sent_timestamp = target_sent_timestamp,
                signal.target_author_aci = %target_author_aci_for_trace,
            );
            let message = DataMessage {
                reaction: Some(Reaction {
                    emoji: Some(emoji),
                    remove: Some(remove),
                    target_author_aci: Some(target_author_aci),
                    target_sent_timestamp: Some(target_sent_timestamp),
                    target_author_aci_binary: None,
                }),
                group_v2: group_context_for_target(&target),
                ..Default::default()
            };
            send_action_with_trace(manager, span, target, message, timestamp).await
        }
        SignalAction::Reply {
            target,
            text,
            quote_timestamp,
            quote_author_aci,
        } => {
            let target_kind = signal_target_kind(&target);
            let target_value = signal_target_value(&target);
            let timestamp = now_epoch_millis();

            let (text, redacted) = name_resolver::sanitize_uuids(&text);
            if redacted > 0 {
                warn!(
                    count = redacted,
                    "redacted UUIDs from outbound signal reply"
                );
            }

            let raw_content = text.clone();
            let quote_author_aci_for_trace = quote_author_aci.clone();
            let span = info_span!(
                "signal_action_send",
                signal.action = "reply",
                signal.target_kind = target_kind,
                signal.target = %target_value,
                signal.timestamp = timestamp,
                signal.raw_content = %raw_content,
                signal.quote_timestamp = quote_timestamp,
                signal.quote_author_aci = %quote_author_aci_for_trace,
            );
            let message = DataMessage {
                body: Some(text),
                quote: Some(Quote {
                    id: Some(quote_timestamp),
                    author_aci: Some(quote_author_aci),
                    text: None,
                    ..Default::default()
                }),
                group_v2: group_context_for_target(&target),
                ..Default::default()
            };
            send_action_with_trace(manager, span, target, message, timestamp).await
        }
        SignalAction::Typing { target, started } => {
            let target_kind = signal_target_kind(&target);
            let target_value = signal_target_value(&target);
            let timestamp = now_epoch_millis();
            let span = info_span!(
                "signal_action_send",
                signal.action = "typing",
                signal.target_kind = target_kind,
                signal.target = %target_value,
                signal.timestamp = timestamp,
                signal.started = started,
            );
            let typing = TypingMessage {
                timestamp: Some(timestamp),
                action: Some(
                    if started {
                        typing_message::Action::Started
                    } else {
                        typing_message::Action::Stopped
                    }
                    .into(),
                ),
                group_id: match &target {
                    SignalTarget::Group { master_key } => Some(master_key.clone()),
                    SignalTarget::Direct(_) => None,
                },
            };
            send_action_with_trace(manager, span, target, typing, timestamp).await
        }
        SignalAction::SendReceipt {
            sender_uuid,
            timestamps,
            receipt_type,
        } => {
            let (action_name, proto_type) = match receipt_type {
                SignalReceiptType::Delivery => (
                    "delivery_receipt",
                    presage::proto::receipt_message::Type::Delivery,
                ),
                SignalReceiptType::Read => {
                    ("read_receipt", presage::proto::receipt_message::Type::Read)
                }
            };
            let timestamp = now_epoch_millis();
            let span = info_span!(
                "signal_action_send",
                signal.action = action_name,
                signal.target_kind = "direct",
                signal.target = %sender_uuid,
                signal.timestamp = timestamp,
                signal.receipt_timestamps = ?timestamps,
            );
            let receipt = presage::proto::ReceiptMessage {
                r#type: Some(proto_type.into()),
                timestamp: timestamps,
            };
            let target = SignalTarget::Direct(sender_uuid);
            send_action_with_trace(manager, span, target, receipt, timestamp).await
        }
        SignalAction::Shutdown => Ok(()),
    }
}

#[allow(clippy::large_futures)]
async fn send_action_with_trace(
    manager: &mut SignalManager,
    span: tracing::Span,
    target: SignalTarget,
    message: impl Into<ContentBody>,
    timestamp: u64,
) -> Result<()> {
    async {
        let result = send_content_to_target(manager, target, message, timestamp).await;
        match &result {
            Ok(()) => debug!("signal action sent"),
            Err(error) => warn!(error = %error, "signal action send failed"),
        }
        result
    }
    .instrument(span)
    .await
}

fn signal_content_body_name(content_body: &ContentBody) -> &'static str {
    match content_body {
        ContentBody::DataMessage(_) => "data_message",
        ContentBody::EditMessage(_) => "edit_message",
        ContentBody::TypingMessage(_) => "typing_message",
        ContentBody::ReceiptMessage(_) => "receipt_message",
        ContentBody::SynchronizeMessage(_) => "synchronize_message",
        _ => "unsupported",
    }
}

/// Build `GroupContextV2` when the target is a group, `None` for direct messages.
///
/// Signal requires every `DataMessage` sent to a group to contain `group_v2`
/// with the group's master key. Without it, recipients see the message as a
/// direct message instead of a group message.
fn group_context_for_target(target: &SignalTarget) -> Option<GroupContextV2> {
    match target {
        SignalTarget::Group { master_key } => Some(GroupContextV2 {
            master_key: Some(master_key.clone()),
            revision: Some(0),
            ..Default::default()
        }),
        SignalTarget::Direct(_) => None,
    }
}

fn signal_target_kind(target: &SignalTarget) -> &'static str {
    match target {
        SignalTarget::Direct(_) => "direct",
        SignalTarget::Group { .. } => "group",
    }
}

fn signal_target_value(target: &SignalTarget) -> String {
    match target {
        SignalTarget::Direct(uuid) => uuid.clone(),
        SignalTarget::Group { master_key } => format!("group:{}", hex::encode(master_key)),
    }
}

#[allow(clippy::large_futures)]
async fn send_content_to_target(
    manager: &mut SignalManager,
    target: SignalTarget,
    message: impl Into<ContentBody>,
    timestamp: u64,
) -> Result<()> {
    match target {
        SignalTarget::Direct(uuid_str) => {
            let uuid = Uuid::parse_str(&uuid_str)
                .with_context(|| format!("invalid signal uuid target: {uuid_str}"))?;
            Box::pin(manager.send_message(ServiceId::Aci(uuid.into()), message, timestamp))
                .await
                .context("failed to send direct signal message")?;
        }
        SignalTarget::Group { master_key } => {
            Box::pin(manager.send_message_to_group(&master_key, message, timestamp))
                .await
                .context("failed to send group signal message")?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Attachment download helpers
// ---------------------------------------------------------------------------

/// Extract `AttachmentPointer`s from any Signal content body variant.
fn extract_attachment_pointers(
    content: &presage::libsignal_service::content::Content,
) -> Vec<&AttachmentPointer> {
    match &content.body {
        ContentBody::DataMessage(dm) => dm.attachments.iter().collect(),
        ContentBody::EditMessage(em) => em
            .data_message
            .as_ref()
            .map(|dm| dm.attachments.iter().collect())
            .unwrap_or_default(),
        ContentBody::SynchronizeMessage(sync) => {
            if let Some(sent) = &sync.sent {
                if let Some(dm) = &sent.message {
                    return dm.attachments.iter().collect();
                }
                if let Some(em) = &sent.edit_message {
                    return em
                        .data_message
                        .as_ref()
                        .map(|dm| dm.attachments.iter().collect())
                        .unwrap_or_default();
                }
            }
            vec![]
        }
        _ => vec![],
    }
}

/// Download each attachment, save to disk, and rewrite the inbound message
/// content to include the saved file paths so the agent can access them.
/// Maximum attachment size we'll save to disk (100 MB).
const MAX_ATTACHMENT_BYTES: usize = 100 * 1024 * 1024;

async fn download_and_rewrite_attachments(
    manager: &SignalManager,
    pointers: &[&AttachmentPointer],
    attachments_dir: &Path,
    timestamp: u64,
    inbound: &mut InboundMessage,
) {
    if let Err(e) = std::fs::create_dir_all(attachments_dir) {
        warn!(error = %e, dir = %attachments_dir.display(), "failed to create attachments directory");
        return;
    }

    for pointer in pointers {
        let original_meta = format_attachment_metadata(pointer);
        let file_name = pointer.file_name.as_deref().unwrap_or("unnamed");

        // Check declared size before downloading
        if let Some(size) = pointer.size
            && size as usize > MAX_ATTACHMENT_BYTES
        {
            warn!(
                file_name,
                declared_size = size,
                max = MAX_ATTACHMENT_BYTES,
                "attachment too large, skipping download"
            );
            let max = MAX_ATTACHMENT_BYTES;
            let replacement = format!(
                "{original_meta}\n[skipped: file too large ({size} bytes, max {max} bytes)]"
            );
            inbound.content = inbound.content.replace(&original_meta, &replacement);
            continue;
        }

        let sanitized = sanitize_filename(file_name);
        let save_name = format!("{timestamp}_{sanitized}");
        let save_name = ensure_media_extension(&save_name, pointer.content_type.as_deref());
        let save_path = attachments_dir.join(&save_name);

        match manager.get_attachment(pointer).await {
            Ok(data) => {
                if data.len() > MAX_ATTACHMENT_BYTES {
                    let actual_size = data.len();
                    let max = MAX_ATTACHMENT_BYTES;
                    warn!(
                        file_name,
                        actual_size, max, "downloaded attachment too large, discarding"
                    );
                    let replacement = format!(
                        "{original_meta}\n[skipped: file too large ({actual_size} bytes, max {max} bytes)]"
                    );
                    inbound.content = inbound.content.replace(&original_meta, &replacement);
                    continue;
                }

                match std::fs::write(&save_path, &data) {
                    Ok(()) => {
                        info!(
                            path = %save_path.display(),
                            size = data.len(),
                            file_name,
                            "saved signal attachment"
                        );
                        let replacement =
                            format!("{original_meta}\n[file saved: {}]", save_path.display());
                        inbound.content = inbound.content.replace(&original_meta, &replacement);
                    }
                    Err(e) => {
                        warn!(error = %e, path = %save_path.display(), "failed to write attachment");
                        let replacement = format!("{original_meta}\n[file save failed: {e}]");
                        inbound.content = inbound.content.replace(&original_meta, &replacement);
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, file_name, "failed to download signal attachment");
                let replacement = format!("{original_meta}\n[download failed: {e}]");
                inbound.content = inbound.content.replace(&original_meta, &replacement);
            }
        }
    }
}

/// Append a file extension when the filename lacks one and the content-type
/// is a recognized MIME type. This ensures downloaded Signal attachments are
/// discoverable by the media-injection pipeline in `coop_core::images`,
/// which requires a recognized extension.
fn ensure_media_extension(name: &str, content_type: Option<&str>) -> String {
    let lower = name.to_lowercase();
    let known_extensions = [
        "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "mp4", "mov", "avi", "mkv", "mp3",
        "m4a", "ogg", "wav", "aac", "flac", "opus", "pdf",
    ];
    if known_extensions
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
    {
        return name.to_owned();
    }

    let ext = match content_type {
        // Images
        Some("image/jpeg") => ".jpg",
        Some("image/png") => ".png",
        Some("image/gif") => ".gif",
        Some("image/webp") => ".webp",
        Some("image/heic") => ".heic",
        Some("image/heif") => ".heif",
        // Audio
        Some("audio/aac") => ".aac",
        Some("audio/mp4" | "audio/x-m4a") => ".m4a",
        Some("audio/mpeg") => ".mp3",
        Some("audio/ogg" | "audio/ogg; codecs=opus") => ".ogg",
        Some("audio/wav" | "audio/x-wav") => ".wav",
        Some("audio/flac") => ".flac",
        Some("audio/opus") => ".opus",
        // Video
        Some("video/mp4") => ".mp4",
        Some("video/quicktime") => ".mov",
        Some("video/x-msvideo") => ".avi",
        Some("video/x-matroska") => ".mkv",
        // Documents
        Some("application/pdf") => ".pdf",
        _ => return name.to_owned(),
    };

    format!("{name}{ext}")
}

/// Sanitize a filename for safe filesystem storage.
fn sanitize_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unnamed".to_owned()
    } else {
        sanitized
    }
}

fn now_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Load contact names from a separate store connection for name resolution.
async fn load_contacts(db_path: &Path) -> Vec<(String, String)> {
    use presage::store::ContentsStore;

    let store = match open_store(db_path).await {
        Ok(store) => store,
        Err(e) => {
            warn!(error = %e, "failed to open store for contact resolution");
            return vec![];
        }
    };

    match store.contacts().await {
        Ok(iter) => iter
            .filter_map(|result| match result {
                Ok(contact) => {
                    let name = contact.name.trim().to_owned();
                    if name.is_empty() {
                        None
                    } else {
                        Some((contact.uuid.to_string(), name))
                    }
                }
                Err(e) => {
                    warn!(error = %e, "skipping unreadable contact");
                    None
                }
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, "failed to load contacts for name resolution");
            vec![]
        }
    }
}

async fn load_registered_manager(db_path: &Path) -> Result<SignalManager> {
    let store = open_store(db_path).await?;
    Manager::load_registered(store)
        .await
        .context("failed to load registered signal account")
}

async fn open_store(db_path: &Path) -> Result<SqliteStore> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let options: SqliteConnectOptions = db_path
        .to_string_lossy()
        .parse()
        .context("invalid signal db path")?;
    let options = options.create_if_missing(true);

    SqliteStore::open_with_options(options, OnNewIdentity::Trust)
        .await
        .context("failed to open signal sqlite store")
}

fn set_health(health: &HealthState, state: ChannelHealth) {
    *health.lock().expect("health mutex poisoned") = state;
}
