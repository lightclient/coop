mod inbound;
#[cfg(test)]
mod target_tests;

use anyhow::{Context, Result};
use async_trait::async_trait;
use coop_core::{
    Channel, ChannelHealth, InboundMessage, OutboundMessage, SessionKey, SessionKind,
    TypingNotifier,
};
use futures::StreamExt;
use inbound::inbound_from_content;
use presage::libsignal_service::content::{ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::proto::data_message::{Quote, Reaction};
use presage::proto::{TypingMessage, typing_message};
use presage::{Manager, store::StateStore};
use presage_store_sqlite::{SqliteConnectOptions, SqliteStore};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, warn};

type SignalManager = Manager<SqliteStore, Registered>;
type HealthState = Arc<Mutex<ChannelHealth>>;

#[derive(Debug, Clone)]
pub enum SignalAction {
    SendText(OutboundMessage),
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
}

#[allow(missing_debug_implementations)]
pub struct SignalChannel {
    id: String,
    inbound_rx: mpsc::Receiver<InboundMessage>,
    action_tx: mpsc::Sender<SignalAction>,
    health: HealthState,
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
        Ok(Self::Direct(value.to_string()))
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
            SessionKind::Main | SessionKind::Isolated(_) => return,
        };

        let _ = self
            .action_tx
            .send(SignalAction::Typing { target, started })
            .await;
    }
}

impl SignalChannel {
    pub async fn connect(db_path: impl AsRef<Path>) -> Result<Self> {
        let manager = load_registered_manager(db_path.as_ref()).await?;

        let (inbound_tx, inbound_rx) = mpsc::channel(64);
        let (action_tx, action_rx) = mpsc::channel(64);
        let health = Arc::new(Mutex::new(ChannelHealth::Healthy));

        start_signal_runtime(manager, inbound_tx, action_rx, health.clone());

        Ok(Self {
            id: "signal".to_string(),
            inbound_rx,
            action_tx,
            health,
        })
    }

    pub fn action_sender(&self) -> mpsc::Sender<SignalAction> {
        self.action_tx.clone()
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

fn start_signal_runtime(
    manager: SignalManager,
    inbound_tx: mpsc::Sender<InboundMessage>,
    action_rx: mpsc::Receiver<SignalAction>,
    health: HealthState,
) {
    std::thread::Builder::new()
        .name("signal-runtime".to_string())
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
                let receive_manager = manager.clone();
                let receive_health = health.clone();
                let send_health = health.clone();

                let receive_task = tokio::task::spawn_local(Box::pin(receive_task(
                    receive_manager,
                    inbound_tx,
                    receive_health,
                )));
                let send_task =
                    tokio::task::spawn_local(Box::pin(send_task(manager, action_rx, send_health)));

                let _ = futures::future::join(receive_task, send_task).await;
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
            .map_err(|_| anyhow::anyhow!("signal action channel closed"))
    }

    async fn probe(&self) -> ChannelHealth {
        self.health.lock().unwrap().clone()
    }
}

#[allow(clippy::large_futures)]
async fn receive_task(
    mut manager: SignalManager,
    inbound_tx: mpsc::Sender<InboundMessage>,
    health: HealthState,
) {
    let mut backoff = Duration::from_secs(1);

    loop {
        match Box::pin(manager.receive_messages()).await {
            Ok(messages) => {
                set_health(&health, ChannelHealth::Healthy);
                backoff = Duration::from_secs(1);

                tokio::pin!(messages);
                while let Some(received) = messages.next().await {
                    if let Received::Content(content) = received
                        && let Some(inbound) = inbound_from_content(&content)
                    {
                        debug!(
                            kind = ?inbound.kind,
                            sender = %inbound.sender,
                            chat_id = ?inbound.chat_id,
                            "received signal inbound"
                        );

                        if inbound_tx.send(inbound).await.is_err() {
                            return;
                        }
                    }
                }

                set_health(
                    &health,
                    ChannelHealth::Degraded("signal receive stream ended".to_string()),
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
        debug!(action = ?action, "sending signal action");

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
        ChannelHealth::Unhealthy("signal sender task stopped".to_string()),
    );
}

#[allow(clippy::large_futures)]
async fn send_signal_action(manager: &mut SignalManager, action: SignalAction) -> Result<()> {
    match action {
        SignalAction::SendText(outbound) => {
            let target = SignalTarget::parse(&outbound.target)?;
            let timestamp = now_epoch_millis();
            let message = DataMessage {
                body: Some(outbound.content),
                ..Default::default()
            };
            send_content_to_target(manager, target, message, timestamp).await
        }
        SignalAction::React {
            target,
            emoji,
            target_author_aci,
            target_sent_timestamp,
            remove,
        } => {
            let timestamp = now_epoch_millis();
            let message = DataMessage {
                reaction: Some(Reaction {
                    emoji: Some(emoji),
                    remove: Some(remove),
                    target_author_aci: Some(target_author_aci),
                    target_sent_timestamp: Some(target_sent_timestamp),
                }),
                ..Default::default()
            };
            send_content_to_target(manager, target, message, timestamp).await
        }
        SignalAction::Reply {
            target,
            text,
            quote_timestamp,
            quote_author_aci,
        } => {
            let timestamp = now_epoch_millis();
            let message = DataMessage {
                body: Some(text),
                quote: Some(Quote {
                    id: Some(quote_timestamp),
                    author_aci: Some(quote_author_aci),
                    text: None,
                    ..Default::default()
                }),
                ..Default::default()
            };
            send_content_to_target(manager, target, message, timestamp).await
        }
        SignalAction::Typing { target, started } => {
            let timestamp = now_epoch_millis();
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
            send_content_to_target(manager, target, typing, timestamp).await
        }
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

fn now_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
    *health.lock().unwrap() = state;
}
