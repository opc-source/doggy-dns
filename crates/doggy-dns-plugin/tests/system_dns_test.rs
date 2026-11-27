#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::builtin::native::NativeAuthority;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_resolver::Resolver;
use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_server::zone_handler::{LookupControlFlow, LookupError, LookupOptions, ZoneHandler};
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;

#[tokio::test]
async fn native_resolves_known_domain() {
    let authority = NativeAuthority::new(256, Duration::from_secs(10), Duration::from_secs(300))
        .await
        .expect("failed to create NativeAuthority");

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Break(Ok(_)) => {
            // Success
        }
        _ => panic!("Expected successful lookup, got non-success result"),
    }
}

#[tokio::test]
async fn native_transport_failure_answers_servfail_instead_of_skipping() {
    // A port that was briefly bound and released is closed, so the datagram is
    // refused. Before the fix this path returned `Skip`, so an all-skipped
    // chain turned a resolver outage into an authoritative NXDOMAIN.
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("failed to bind probe socket");
    let closed_port = probe
        .local_addr()
        .expect("failed to read probe addr")
        .port();
    drop(probe);

    let mut server = NameServerConfig::udp(IpAddr::V4(Ipv4Addr::LOCALHOST));
    server
        .connections
        .first_mut()
        .expect("udp nameserver has a connection")
        .port = closed_port;

    let mut builder = Resolver::builder_with_config(
        ResolverConfig::from_name_servers(vec![server]),
        TokioRuntimeProvider::default(),
    );
    let options = builder.options_mut();
    options.timeout = Duration::from_millis(250);
    options.attempts = 1;
    let resolver = builder.build().expect("failed to build test resolver");
    let authority = NativeAuthority::from_resolver(resolver);

    let name = LowerName::from(Name::from_str("unreachable.example.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
        ),
        "a resolver transport error must answer SERVFAIL, not fall through"
    );
}
