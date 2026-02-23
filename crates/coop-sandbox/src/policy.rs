use std::path::PathBuf;

/// Policy for a sandboxed command execution.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Directory mounted read-write as the working directory.
    pub workspace: PathBuf,
    /// Whether to allow network access. Default: false (empty network namespace).
    pub allow_network: bool,
    /// Memory limit in bytes. 0 = no limit.
    pub memory_limit: u64,
    /// Max number of PIDs (fork bomb protection). 0 = no limit.
    pub pids_limit: u32,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("."),
            allow_network: false,
            memory_limit: 2 * 1024 * 1024 * 1024, // 2 GiB
            pids_limit: 512,
        }
    }
}

/// Output from a sandboxed command execution.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Information about sandbox capabilities on this platform.
#[derive(Debug, Clone)]
pub struct SandboxInfo {
    pub name: String,
    pub capabilities: SandboxCapabilities,
}

/// What sandboxing features are available.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default)]
pub struct SandboxCapabilities {
    pub user_namespaces: bool,
    pub network_namespaces: bool,
    pub landlock: bool,
    pub seccomp: bool,
    pub cgroups_v2: bool,
}

/// Parse a memory size string like "2g", "512m", "1024k" into bytes.
pub fn parse_memory_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || s == "0" {
        return Ok(0);
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('g') {
        (n, 1024 * 1024 * 1024u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1024 * 1024u64)
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1024u64)
    } else {
        (s.as_str(), 1u64)
    };

    let num: u64 = num_str
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid memory size '{s}': {e}"))?;

    Ok(num * multiplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_memory_size_bytes() {
        assert_eq!(parse_memory_size("1024").expect("valid"), 1024);
    }

    #[test]
    fn parse_memory_size_kilobytes() {
        assert_eq!(parse_memory_size("512k").expect("valid"), 512 * 1024);
    }

    #[test]
    fn parse_memory_size_megabytes() {
        assert_eq!(parse_memory_size("256m").expect("valid"), 256 * 1024 * 1024);
    }

    #[test]
    fn parse_memory_size_gigabytes() {
        assert_eq!(
            parse_memory_size("2g").expect("valid"),
            2 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn parse_memory_size_zero() {
        assert_eq!(parse_memory_size("0").expect("valid"), 0);
    }

    #[test]
    fn parse_memory_size_case_insensitive() {
        assert_eq!(
            parse_memory_size("4G").expect("valid"),
            4 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn parse_memory_size_invalid() {
        assert!(parse_memory_size("abc").is_err());
    }

    #[test]
    fn default_policy() {
        let policy = SandboxPolicy::default();
        assert!(!policy.allow_network);
        assert_eq!(policy.memory_limit, 2 * 1024 * 1024 * 1024);
        assert_eq!(policy.pids_limit, 512);
    }
}
