#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_mcp::state::McpState;
use std::path::PathBuf;

#[test]
fn instance_id_contains_host_and_pid() {
    let state = McpState::from_parts(
        "dns-node-1".into(),
        4242,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        Vec::new(),
        PathBuf::from("config/doggy-dns.toml"),
    );
    assert_eq!(state.instance_id, "dns-node-1-4242");
    assert_eq!(state.host, "dns-node-1");
    assert_eq!(state.pid, 4242);
}

#[test]
fn state_preserves_protocols_and_listen_addr() {
    let state = McpState::from_parts(
        "test-host".into(),
        1234,
        "0.2.0".into(),
        Utc::now(),
        "127.0.0.1:5353".into(),
        vec!["udp".into(), "tcp".into(), "dot".into(), "doh".into()],
        Vec::new(),
        PathBuf::from("/etc/doggy-dns.toml"),
    );
    assert_eq!(state.version, "0.2.0");
    assert_eq!(state.listen_addr, "127.0.0.1:5353");
    assert_eq!(state.protocols.len(), 4);
    assert_eq!(state.protocols[2], "dot");
}

#[test]
fn state_stores_config_path() {
    let state = McpState::from_parts(
        "h".into(),
        1,
        "0.0.0".into(),
        Utc::now(),
        "0.0.0.0:53".into(),
        vec![],
        Vec::new(),
        PathBuf::from("/custom/path.toml"),
    );
    assert_eq!(state.config_path, PathBuf::from("/custom/path.toml"));
}
