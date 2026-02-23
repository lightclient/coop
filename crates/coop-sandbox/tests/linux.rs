//! Integration tests for the Linux sandbox.
//! Gated behind `COOP_SANDBOX_TEST=1` because they require unprivileged user
//! namespaces to be enabled on the host kernel.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

use coop_sandbox::{SandboxPolicy, exec, probe};
use std::time::Duration;

fn should_run() -> bool {
    std::env::var("COOP_SANDBOX_TEST").is_ok_and(|v| v == "1")
}

fn test_policy(workspace: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy {
        workspace: workspace.to_path_buf(),
        allow_network: false,
        memory_limit: 512 * 1024 * 1024, // 512 MB
        pids_limit: 64,
    }
}

#[test]
fn probe_reports_capabilities() {
    if !should_run() {
        return;
    }
    let info = probe().expect("probe should succeed");
    assert!(info.name.contains("linux"));
    assert!(info.capabilities.user_namespaces);
}

#[tokio::test]
async fn basic_exec() {
    if !should_run() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let policy = test_policy(dir.path());
    let output = exec(&policy, "echo hello", Duration::from_secs(10))
        .await
        .expect("exec should succeed");
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.trim().contains("hello"));
}

#[tokio::test]
async fn workspace_write_persists() {
    if !should_run() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let policy = test_policy(dir.path());

    let output = exec(
        &policy,
        "echo test-data > testfile.txt",
        Duration::from_secs(10),
    )
    .await
    .expect("exec should succeed");
    assert_eq!(output.exit_code, 0);

    let content = std::fs::read_to_string(dir.path().join("testfile.txt")).expect("read file");
    assert!(content.contains("test-data"));
}

#[tokio::test]
async fn network_isolation() {
    if !should_run() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let policy = test_policy(dir.path());

    let output = exec(
        &policy,
        "curl -s --connect-timeout 2 http://example.com 2>&1; echo exit=$?",
        Duration::from_secs(10),
    )
    .await
    .expect("exec should succeed");

    let combined = format!("{}{}", output.stdout, output.stderr);
    let has_error = combined.contains("Could not resolve")
        || combined.contains("Network is unreachable")
        || combined.contains("exit=6")
        || combined.contains("exit=7")
        || combined.contains("not found")
        || output.exit_code != 0;
    assert!(
        has_error,
        "expected network failure, got: stdout={} stderr={}",
        output.stdout, output.stderr
    );
}

#[tokio::test]
async fn timeout_kills_process() {
    if !should_run() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let policy = test_policy(dir.path());

    let output = exec(&policy, "sleep 999", Duration::from_secs(2))
        .await
        .expect("exec should succeed");
    assert_ne!(output.exit_code, 0);
    assert!(
        output.stderr.contains("timed out"),
        "expected timeout message, got: {}",
        output.stderr
    );
}

#[tokio::test]
async fn workspace_file_readable() {
    if !should_run() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("data.txt"), "secret-data").expect("write file");

    let policy = test_policy(dir.path());
    let output = exec(&policy, "cat data.txt", Duration::from_secs(10))
        .await
        .expect("exec should succeed");
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.contains("secret-data"));
}

#[tokio::test]
async fn exit_code_propagated() {
    if !should_run() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let policy = test_policy(dir.path());
    let output = exec(&policy, "exit 42", Duration::from_secs(10))
        .await
        .expect("exec should succeed");
    assert_eq!(output.exit_code, 42);
}
