use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use coop_core::{Channel, ChannelHealth, InboundMessage, OutboundMessage};
use futures::StreamExt;
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::{Manager, store::StateStore};
use presage_store_sqlite::{SqliteConnectOptions, SqliteStore};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::warn;

type SignalManager = Manager<SqliteStore, Registered>;
type HealthState = Arc<Mutex<ChannelHealth>>;

#[allow(missing_debug_implementations)]
pub struct SignalChannel {
    id: String,
    inbound_rx: mpsc::Receiver<InboundMessage>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
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

impl SignalChannel {
    pub async fn connect(db_path: impl AsRef<Path>) -> Result<Self> {
        let manager = load_registered_manager(db_path.as_ref()).await?;

        let (inbound_tx, inbound_rx) = mpsc::channel(64);
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        let health = Arc::new(Mutex::new(ChannelHealth::Healthy));

        start_signal_runtime(manager, inbound_tx, outbound_rx, health.clone());

        Ok(Self {
            id: "signal".to_string(),
            inbound_rx,
            outbound_tx,
            health,
        })
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
    outbound_rx: mpsc::Receiver<OutboundMessage>,
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
                let send_task = tokio::task::spawn_local(Box::pin(send_task(
                    manager,
                    outbound_rx,
                    send_health,
                )));

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
        self.outbound_tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("signal outbound channel closed"))
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
                        && inbound_tx.send(inbound).await.is_err()
                    {
                        return;
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
    mut outbound_rx: mpsc::Receiver<OutboundMessage>,
    health: HealthState,
) {
    while let Some(outbound) = outbound_rx.recv().await {
        match send_outbound_message(&mut manager, outbound).await {
            Ok(()) => set_health(&health, ChannelHealth::Healthy),
            Err(error) => {
                warn!(error = %error, "failed to send signal message");
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
async fn send_outbound_message(
    manager: &mut SignalManager,
    outbound: OutboundMessage,
) -> Result<()> {
    let target = SignalTarget::parse(&outbound.target)?;
    let timestamp = now_epoch_millis();

    let message = DataMessage {
        body: Some(outbound.content),
        ..Default::default()
    };

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

fn inbound_from_content(content: &Content) -> Option<InboundMessage> {
    let data_message = extract_supported_data_message(&content.body)?;
    let text = data_message.body.as_deref()?.trim();

    if text.is_empty() {
        return None;
    }

    let sender = content.metadata.sender.raw_uuid().to_string();
    let (chat_id, is_group, reply_to) = if let Some(master_key) = data_message
        .group_v2
        .as_ref()
        .and_then(|group| group.master_key.as_ref())
    {
        let target = format!("group:{}", hex::encode(master_key));
        (Some(target.clone()), true, Some(target))
    } else {
        (None, false, Some(sender.clone()))
    };

    Some(InboundMessage {
        channel: "signal".to_string(),
        sender,
        content: text.to_string(),
        chat_id,
        is_group,
        timestamp: from_epoch_millis(content.metadata.timestamp),
        reply_to,
    })
}

fn extract_supported_data_message(content: &ContentBody) -> Option<&DataMessage> {
    match content {
        ContentBody::DataMessage(data_message) => Some(data_message),
        ContentBody::SynchronizeMessage(sync) => sync.sent.as_ref()?.message.as_ref(),
        _ => None,
    }
}

fn from_epoch_millis(timestamp: u64) -> DateTime<Utc> {
    i64::try_from(timestamp)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_target() {
        let target = SignalTarget::parse("alice-uuid").unwrap();
        assert_eq!(target, SignalTarget::Direct("alice-uuid".to_string()));
    }

    #[test]
    fn parse_prefixed_direct_target() {
        let target = SignalTarget::parse("signal:alice-uuid").unwrap();
        assert_eq!(target, SignalTarget::Direct("alice-uuid".to_string()));
    }

    #[test]
    fn parse_group_target() {
        let target = SignalTarget::parse(
            "group:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        )
        .unwrap();
        assert_eq!(
            target,
            SignalTarget::Group {
                master_key: hex::decode(
                    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
                )
                .unwrap(),
            }
        );
    }

    #[test]
    fn reject_invalid_group_key() {
        assert!(SignalTarget::parse("group:not-hex").is_err());
    }

    #[test]
    fn reject_wrong_group_key_size() {
        assert!(SignalTarget::parse("group:deadbeef").is_err());
    }
}
