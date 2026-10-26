#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_mcp::McpServerConfig;
use doggy_dns_mcp::state::McpState;
use doggy_dns_mcp::transport;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

fn test_state() -> Arc<McpState> {
    Arc::new(McpState::from_parts(
        "dns-node-1".into(),
        4242,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        Vec::new(),
        PathBuf::from("config/doggy-dns.toml"),
    ))
}

#[tokio::test]
async fn occupied_port_returns_error_without_panicking() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();
    let err = transport::start(
        test_state(),
        McpServerConfig {
            listen_addr: "127.0.0.1".into(),
            port: addr.port(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("address already in use") || msg.contains("bind") || msg.contains("occupied"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn start_with_port_zero_assigns_random_port() {
    let handle = transport::start(
        test_state(),
        McpServerConfig {
            port: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_ne!(handle.addr.port(), 0);
    handle.cancel.cancel();
}
