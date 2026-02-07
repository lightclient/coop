use crate::protocol::{ClientMessage, ServerMessage};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

#[allow(missing_debug_implementations)]
pub struct IpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl IpcServer {
    pub fn bind(socket_path: impl AsRef<Path>) -> Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();

        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create socket directory {}", parent.display())
            })?;
        }

        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("failed to remove {}", socket_path.display()))?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind {}", socket_path.display()))?;

        Ok(Self {
            socket_path,
            listener,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub async fn accept(&self) -> Result<IpcConnection> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .context("failed to accept IPC connection")?;
        Ok(IpcConnection::new(stream))
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[allow(missing_debug_implementations)]
pub struct IpcConnection {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
}

impl IpcConnection {
    fn new(stream: UnixStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half).lines(),
            writer: write_half,
        }
    }

    pub async fn recv(&mut self) -> Result<ClientMessage> {
        loop {
            let line = self
                .reader
                .next_line()
                .await
                .context("failed to read client message")?
                .ok_or_else(|| anyhow::anyhow!("client disconnected"))?;

            if line.trim().is_empty() {
                continue;
            }

            return serde_json::from_str(&line).context("failed to decode client message");
        }
    }

    pub async fn send(&mut self, message: ServerMessage) -> Result<()> {
        let encoded = serde_json::to_string(&message).context("failed to encode server message")?;
        self.writer
            .write_all(encoded.as_bytes())
            .await
            .context("failed to write server message")?;
        self.writer
            .write_all(b"\n")
            .await
            .context("failed to write message delimiter")?;
        self.writer
            .flush()
            .await
            .context("failed to flush server message")?;
        Ok(())
    }
}
