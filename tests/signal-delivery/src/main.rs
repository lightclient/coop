use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::{Parser, Subcommand};
use futures::{pin_mut, StreamExt};
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::libsignal_service::prelude::Uuid;
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::Manager;
use presage_store_sqlite::SqliteStore;
use tracing::{error, info, warn};

#[derive(Parser)]
#[clap(about = "Signal delivery test binary using presage directly")]
struct Args {
    /// Path to the presage SQLite database
    #[clap(long, default_value = "../../db/signal.db")]
    db_path: String,

    #[clap(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send a single message to a recipient
    Send {
        /// UUID of the recipient
        uuid: Uuid,
        /// Message text to send
        message: String,
    },
    /// Send N messages at regular intervals
    SendLoop {
        /// UUID of the recipient
        uuid: Uuid,
        /// Number of messages to send
        #[clap(long, default_value = "5")]
        count: u32,
        /// Interval between messages in seconds
        #[clap(long, default_value = "10")]
        interval_secs: u64,
    },
    /// Connect and listen for incoming messages
    Monitor,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::metadata::LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    let args = Args::parse();

    info!(db_path = %args.db_path, "opening presage store");
    let store = SqliteStore::open(&args.db_path, OnNewIdentity::Trust).await?;
    let manager = Manager::load_registered(store).await?;
    info!("presage manager loaded");

    let local = tokio::task::LocalSet::new();
    local.run_until(run(args.command, manager)).await
}

async fn run(
    command: Cmd,
    manager: Manager<SqliteStore, Registered>,
) -> anyhow::Result<()> {
    match command {
        Cmd::Send { uuid, message } => {
            // Clone manager for background receive (needed for websocket to work)
            let manager_recv = manager.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = drain_and_receive(manager_recv).await {
                    warn!(error = %e, "background receive ended");
                }
            });

            let mut manager = manager;
            send_one(&mut manager, uuid, &message).await?;
        }
        Cmd::SendLoop {
            uuid,
            count,
            interval_secs,
        } => {
            let manager_recv = manager.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = drain_and_receive(manager_recv).await {
                    warn!(error = %e, "background receive ended");
                }
            });

            let mut manager = manager;
            for i in 1..=count {
                let msg = format!("test-{i}-{}", now_millis());
                info!(i, count, msg = %msg, "sending message");
                let start = std::time::Instant::now();
                match send_one(&mut manager, uuid, &msg).await {
                    Ok(()) => {
                        info!(
                            i,
                            count,
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            "✅ message sent"
                        );
                    }
                    Err(e) => {
                        error!(
                            i,
                            count,
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            error = %e,
                            "❌ send failed"
                        );
                    }
                }
                if i < count {
                    tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                }
            }
        }
        Cmd::Monitor => {
            info!("starting message monitor");
            let mut manager = manager;
            let messages = manager
                .receive_messages()
                .await
                .context("failed to initialize messages stream")?;
            pin_mut!(messages);

            while let Some(content) = messages.next().await {
                match content {
                    Received::QueueEmpty => {
                        info!("queue empty — now listening for new messages");
                    }
                    Received::Contacts => {
                        info!("received contacts sync");
                    }
                    Received::Content(content) => {
                        print_content(&content);
                    }
                }
            }
        }
    }

    Ok(())
}

async fn drain_and_receive(
    mut manager: Manager<SqliteStore, Registered>,
) -> anyhow::Result<()> {
    info!("starting background message drain");
    let messages = manager
        .receive_messages()
        .await
        .context("failed to initialize messages stream")?;
    pin_mut!(messages);

    while let Some(content) = messages.next().await {
        match content {
            Received::QueueEmpty => {
                info!("background drain: queue empty");
            }
            Received::Contacts => {
                info!("background drain: contacts sync");
            }
            Received::Content(content) => {
                print_content(&content);
            }
        }
    }
    Ok(())
}

async fn send_one(
    manager: &mut Manager<SqliteStore, Registered>,
    uuid: Uuid,
    message: &str,
) -> anyhow::Result<()> {
    let timestamp = now_millis();

    let data_message = DataMessage {
        body: Some(message.to_string()),
        timestamp: Some(timestamp),
        ..Default::default()
    };

    let content_body = ContentBody::from(data_message);

    info!(
        recipient = %uuid,
        timestamp,
        msg = %message,
        "sending message"
    );

    let start = std::time::Instant::now();

    // Use a timeout to prevent infinite hangs
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        manager.send_message(ServiceId::Aci(uuid.into()), content_body, timestamp),
    )
    .await;

    match result {
        Ok(Ok(())) => {
            info!(
                elapsed_ms = start.elapsed().as_millis() as u64,
                "message sent successfully"
            );
            Ok(())
        }
        Ok(Err(e)) => {
            let err_str = format!("{e}");
            // The message to the recipient may have been sent even if the self-sync
            // fails (e.g. due to websocket closing during sync). The recipient listener
            // is the authoritative check for delivery.
            if err_str.contains("WebSocket closing") || err_str.contains("WsClosing") {
                warn!(
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    error = %e,
                    "send completed with websocket error (message likely delivered, sync to self may have failed)"
                );
                // Return OK — the message to recipient was likely sent before the sync
                // failed. The test script checks the recipient listener for actual delivery.
                Ok(())
            } else {
                error!(
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    error = %e,
                    "send_message returned error"
                );
                Err(anyhow::anyhow!("send_message failed: {e}"))
            }
        }
        Err(_) => {
            error!(
                elapsed_ms = start.elapsed().as_millis() as u64,
                "send_message timed out after 60s"
            );
            Err(anyhow::anyhow!("send_message timed out after 60s"))
        }
    }
}

fn print_content(content: &Content) {
    let sender = content.metadata.sender.raw_uuid();
    let ts = content.metadata.timestamp;

    match &content.body {
        ContentBody::DataMessage(DataMessage {
            body: Some(body), ..
        }) => {
            println!("[{ts}] From {sender}: {body}");
            info!(sender = %sender, timestamp = ts, body = %body, "received message");
        }
        ContentBody::SynchronizeMessage(sync) => {
            if let Some(sent) = &sync.sent {
                if let Some(dm) = &sent.message {
                    if let Some(body) = &dm.body {
                        println!("[{ts}] Sent sync: {body}");
                        info!(timestamp = ts, body = %body, "sent sync message");
                    }
                }
            }
        }
        other => {
            let type_name = match other {
                ContentBody::NullMessage(_) => "NullMessage",
                ContentBody::CallMessage(_) => "CallMessage",
                ContentBody::TypingMessage(_) => "TypingMessage",
                ContentBody::ReceiptMessage(_) => "ReceiptMessage",
                ContentBody::StoryMessage(_) => "StoryMessage",
                ContentBody::PniSignatureMessage(_) => "PniSignatureMessage",
                ContentBody::EditMessage(_) => "EditMessage",
                _ => "Unknown",
            };
            info!(sender = %sender, timestamp = ts, content_type = type_name, "received non-data message");
        }
    }
}
