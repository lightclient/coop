#![allow(clippy::unwrap_used)]

use coop_sandbox::policy::{SandboxPolicy, parse_memory_size};

#[test]
fn default_policy_has_sane_defaults() {
    let policy = SandboxPolicy::default();
    assert!(!policy.allow_network);
    assert!(policy.memory_limit > 0);
    assert!(policy.pids_limit > 0);
}

#[test]
fn parse_memory_sizes() {
    assert_eq!(
        parse_memory_size("2g").expect("valid"),
        2 * 1024 * 1024 * 1024
    );
    assert_eq!(parse_memory_size("512m").expect("valid"), 512 * 1024 * 1024);
    assert_eq!(parse_memory_size("1024k").expect("valid"), 1024 * 1024);
    assert_eq!(parse_memory_size("4096").expect("valid"), 4096);
    assert_eq!(parse_memory_size("0").expect("valid"), 0);
    assert!(parse_memory_size("abc").is_err());
}
