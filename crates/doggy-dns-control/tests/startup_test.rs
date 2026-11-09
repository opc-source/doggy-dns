#![allow(clippy::disallowed_methods)]

mod common;

use common::test_state;
use doggy_dns_control::ControlConfig;
use doggy_dns_control::live::LiveState;
use doggy_dns_control::transport;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::test]
async fn occupied_port_returns_error_without_panicking() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();
    let err = transport::start(
        Arc::new(test_state(Vec::new(), "")),
        Arc::new(LiveState::new()),
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
        Arc::new(test_state(Vec::new(), "")),
        Arc::new(LiveState::new()),
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
        Arc::new(test_state(Vec::new(), "")),
        Arc::new(LiveState::new()),
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

fn retry_cfg(port: u16) -> ControlConfig {
    ControlConfig {
        listen_addr: "127.0.0.1".into(),
        port,
        ..Default::default()
    }
}

/// `None` when nothing answered in time. A probe has to be an actual request:
/// a held port still finishes the TCP handshake out of the holder's backlog
/// without ever accepting, so a successful `connect` proves nothing about who
/// owns the address.
async fn healthz(addr: std::net::SocketAddr, in_time: Duration) -> Option<u16> {
    reqwest::Client::builder()
        .timeout(in_time)
        .build()
        .ok()?
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .ok()
        .map(|resp| resp.status().as_u16())
}

/// The restart case the retry exists for: the predecessor still holds the port,
/// so the successor serves nothing — and then binds by itself once released.
#[tokio::test]
async fn start_retrying_serves_after_the_holder_releases_the_port() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();

    let handle = transport::start_retrying(
        Arc::new(test_state(Vec::new(), "")),
        Arc::new(LiveState::new()),
        retry_cfg(addr.port()),
        Duration::from_millis(20),
        true,
    )
    .unwrap();
    assert_eq!(handle.addr, addr);

    assert_eq!(
        healthz(addr, Duration::from_millis(200)).await,
        None,
        "nothing may be served while the predecessor holds the port"
    );

    drop(occupied);
    // A few intervals is plenty for the pending sleep to land on a free port.
    let mut served = None;
    for _ in 0..50 {
        served = healthz(addr, Duration::from_millis(200)).await;
        if served.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(served, Some(200), "the retry never served the health probe");

    handle.cancel.cancel();
    handle.wait().await.unwrap();
}

/// Cancelling during the wait has to end the task rather than leave it sleeping
/// on a port nobody will free: the binary awaits this handle on shutdown.
#[tokio::test]
async fn start_retrying_stops_while_waiting_when_cancelled() {
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = occupied.local_addr().unwrap();

    let handle = transport::start_retrying(
        Arc::new(test_state(Vec::new(), "")),
        Arc::new(LiveState::new()),
        retry_cfg(addr.port()),
        Duration::from_secs(3600),
        true,
    )
    .unwrap();
    handle.cancel.cancel();

    tokio::time::timeout(Duration::from_secs(5), handle.wait())
        .await
        .expect("the retrying task should stop on cancel")
        .unwrap();
    assert_eq!(
        healthz(addr, Duration::from_millis(200)).await,
        None,
        "the holder was still bound, so a cancelled retry must not have served"
    );
}
