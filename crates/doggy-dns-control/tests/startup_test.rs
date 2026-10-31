#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_control::ControlConfig;
use doggy_dns_control::live::LiveState;
use doggy_dns_control::state::McpState;
use doggy_dns_control::transport;
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
        "",
    ))
}

#[tokio::test]
async fn occupied_port_returns_error_without_panicking() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();
    let err = transport::start(
        test_state(),
        Arc::new(LiveState::new()),
        None,
        ControlConfig {
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
async fn start_binds_ipv6_loopback() {
    let probe = TcpListener::bind("[::1]:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let handle = transport::start(
        test_state(),
        Arc::new(LiveState::new()),
        None,
        ControlConfig {
            listen_addr: "::1".into(),
            port,
            ..Default::default()
        },
    )
    .await
    .expect("IPv6 loopback should bind");
    assert_eq!(
        handle.addr,
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))
    );
    handle.cancel.cancel();
}

#[tokio::test]
async fn start_with_port_zero_assigns_random_port() {
    let handle = transport::start(
        test_state(),
        Arc::new(LiveState::new()),
        None,
        ControlConfig {
            port: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_ne!(handle.addr.port(), 0);
    handle.cancel.cancel();
}
