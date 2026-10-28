#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType, rdata::A};
use hickory_server::zone_handler::{LookupControlFlow, LookupError, LookupOptions, ZoneHandler};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;

#[tokio::test]
async fn forward_resolves_known_domain() {
    let authority = ForwardAuthority::new(
        vec!["8.8.8.8:53".to_string()],
        1024,
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await
    .expect("failed to create ForwardAuthority");

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_)) => {
            // Success: resolved
        }
        _ => panic!("Expected successful lookup, got non-success result"),
    }
}

/// Spawn a UDP DNS server that answers every query with `rcode` and,
/// optionally, a single A record.
async fn spawn_mock_dns(rcode: ResponseCode, answer: Option<(Ipv4Addr, u32)>) -> SocketAddr {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("failed to bind mock DNS socket");
    let addr = socket.local_addr().expect("failed to read mock DNS addr");

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, src)) = socket.recv_from(&mut buf).await else {
                break;
            };
            let request = match Message::from_vec(&buf[..n]) {
                Ok(request) => request,
                Err(_) => continue,
            };
            let Some(query) = request.queries.first().cloned() else {
                continue;
            };

            let mut response =
                Message::new(request.metadata.id, MessageType::Response, OpCode::Query);
            response.metadata.recursion_desired = request.metadata.recursion_desired;
            response.metadata.recursion_available = true;
            response.metadata.response_code = rcode;
            response.add_query(query.clone());
            if let Some((ip, ttl)) = answer {
                response.add_answer(Record::from_rdata(
                    query.name().clone(),
                    ttl,
                    RData::A(A(ip)),
                ));
            }

            let bytes = response.to_vec().expect("failed to encode mock response");
            let _ = socket.send_to(&bytes, src).await;
        }
    });

    addr
}

async fn forward_to(upstream: SocketAddr) -> ForwardAuthority {
    ForwardAuthority::new(
        vec![upstream.to_string()],
        128,
        Duration::from_secs(1),
        Duration::from_secs(60),
    )
    .await
    .expect("failed to create ForwardAuthority")
}

#[tokio::test]
async fn forward_uses_configured_upstream_port() {
    let upstream = spawn_mock_dns(
        ResponseCode::NoError,
        Some((Ipv4Addr::new(192, 0, 2, 7), 30)),
    )
    .await;
    let authority = forward_to(upstream).await;

    let name = LowerName::from(Name::from_str("example.test.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
            assert_eq!(lookup.iter().count(), 1);
        }
        _ => panic!("Expected successful lookup through the configured port"),
    }
}

#[tokio::test]
async fn forward_rejects_empty_upstream_list() {
    let result =
        ForwardAuthority::new(vec![], 128, Duration::from_secs(1), Duration::from_secs(60)).await;
    assert!(result.is_err(), "an empty upstream list must be rejected");
}

#[tokio::test]
async fn forward_rejects_invalid_upstream() {
    let result = ForwardAuthority::new(
        vec!["not-an-ip".to_string()],
        128,
        Duration::from_secs(1),
        Duration::from_secs(60),
    )
    .await;
    assert!(result.is_err(), "a non-IP upstream must be rejected");
}

#[tokio::test]
async fn forward_accepts_bare_ip() {
    let result = ForwardAuthority::new(
        vec!["8.8.8.8".to_string()],
        128,
        Duration::from_secs(1),
        Duration::from_secs(60),
    )
    .await;
    assert!(result.is_ok(), "a bare IP should default to port 53");
}

#[tokio::test]
async fn forward_maps_upstream_nxdomain() {
    let upstream = spawn_mock_dns(ResponseCode::NXDomain, None).await;
    let authority = forward_to(upstream).await;

    let name = LowerName::from(Name::from_str("missing.test.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
        ),
        "upstream NXDOMAIN must stay NXDOMAIN, not become SERVFAIL"
    );
}

#[tokio::test]
async fn forward_maps_upstream_nodata() {
    let upstream = spawn_mock_dns(ResponseCode::NoError, None).await;
    let authority = forward_to(upstream).await;

    let name = LowerName::from(Name::from_str("no-data.test.").unwrap());
    let result = authority
        .lookup(&name, RecordType::AAAA, None, LookupOptions::default())
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::NameExists))
        ),
        "upstream NOERROR/empty must be NODATA"
    );
}

#[tokio::test]
async fn forward_maps_upstream_servfail() {
    let upstream = spawn_mock_dns(ResponseCode::ServFail, None).await;
    let authority = forward_to(upstream).await;

    let name = LowerName::from(Name::from_str("broken.test.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
        ),
        "upstream SERVFAIL must stay SERVFAIL"
    );
}
