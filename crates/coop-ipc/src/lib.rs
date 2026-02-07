pub mod client;
pub mod protocol;
pub mod server;

pub use client::{IpcClient, IpcReader, IpcWriter};
pub use protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage};
pub use server::{IpcConnection, IpcServer};

use std::path::PathBuf;

pub fn socket_path(agent_id: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
    socket_path_with_runtime_dir(agent_id, runtime_dir.as_deref())
}

fn socket_path_with_runtime_dir(agent_id: &str, runtime_dir: Option<&str>) -> PathBuf {
    let safe_agent_id: String = agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();

    if let Some(runtime_dir) = runtime_dir
        && !runtime_dir.is_empty()
    {
        return PathBuf::from(runtime_dir)
            .join("coop")
            .join(format!("{safe_agent_id}.sock"));
    }

    PathBuf::from(format!("/tmp/coop-{safe_agent_id}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_falls_back_to_tmp() {
        let path = socket_path_with_runtime_dir("agent", None);
        assert_eq!(path, PathBuf::from("/tmp/coop-agent.sock"));
    }

    #[test]
    fn socket_path_uses_runtime_dir_when_available() {
        let path = socket_path_with_runtime_dir("agent", Some("/run/user/1000"));
        assert_eq!(path, PathBuf::from("/run/user/1000/coop/agent.sock"));
    }

    #[test]
    fn socket_path_sanitizes_agent_id() {
        let path = socket_path_with_runtime_dir("agent/main", None);
        assert_eq!(path, PathBuf::from("/tmp/coop-agent-main.sock"));
    }
}
