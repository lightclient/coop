use crate::protocol::{ClientMessage, ServerMessage};
use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

#[allow(missing_debug_implementations)]
pub struct IpcClient {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

#[allow(missing_debug_implementations)]
pub struct IpcReader {
    reader: Lines<BufReader<OwnedReadHalf>>,
}

#[allow(missing_debug_implementations)]
pub struct IpcWriter {
    writer: OwnedWriteHalf,
}

impl IpcClient {
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed to connect to {}", socket_path.display()))?;
        let (read_half, write_half) = stream.into_split();

        Ok(Self {
            reader: BufReader::new(read_half).lines(),
            writer: write_half,
        })
    }

    pub fn into_split(self) -> (IpcReader, IpcWriter) {
        (
            IpcReader {
                reader: self.reader,
            },
            IpcWriter {
                writer: self.writer,
            },
        )
    }

    pub async fn send(&mut self, message: ClientMessage) -> Result<()> {
        send_message(&mut self.writer, &message).await
    }

    pub async fn recv(&mut self) -> Result<ServerMessage> {
        recv_message(&mut self.reader).await
    }
}

impl IpcReader {
    pub async fn recv(&mut self) -> Result<ServerMessage> {
        recv_message(&mut self.reader).await
    }
}

impl IpcWriter {
    pub async fn send(&mut self, message: ClientMessage) -> Result<()> {
        send_message(&mut self.writer, &message).await
    }
}

async fn send_message(writer: &mut OwnedWriteHalf, message: &ClientMessage) -> Result<()> {
    let encoded = serde_json::to_string(message).context("failed to encode client message")?;
    writer
        .write_all(encoded.as_bytes())
        .await
        .context("failed to write client message")?;
    writer
        .write_all(b"\n")
        .await
        .context("failed to write message delimiter")?;
    writer
        .flush()
        .await
        .context("failed to flush client message")?;
    Ok(())
}

async fn recv_message(reader: &mut Lines<BufReader<OwnedReadHalf>>) -> Result<ServerMessage> {
    loop {
        let line = reader
            .next_line()
            .await
            .context("failed to read server message")?
            .ok_or_else(|| anyhow::anyhow!("ipc connection closed"))?;

        if line.trim().is_empty() {
            continue;
        }

        return serde_json::from_str(&line).context("failed to decode server message");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PROTOCOL_VERSION;
    use crate::server::IpcServer;

    fn temp_socket(name: &str) -> std::path::PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "coop-ipc-{name}-{}-{millis}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn client_server_round_trip() {
        let socket = temp_socket("roundtrip");
        let server = IpcServer::bind(&socket).unwrap();

        let server_task = tokio::spawn(async move {
            let mut connection = server.accept().await.unwrap();
            let message = connection.recv().await.unwrap();
            assert_eq!(
                message,
                ClientMessage::Hello {
                    version: PROTOCOL_VERSION
                }
            );
            connection
                .send(ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    agent_id: "coop".into(),
                })
                .await
                .unwrap();
        });

        let mut client = IpcClient::connect(&socket).await.unwrap();
        client
            .send(ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            })
            .await
            .unwrap();
        let response = client.recv().await.unwrap();

        assert_eq!(
            response,
            ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                agent_id: "coop".into(),
            }
        );

        server_task.await.unwrap();
    }
}
