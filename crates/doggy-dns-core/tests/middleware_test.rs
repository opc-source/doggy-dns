#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::Request;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

fn make_dns_request(domain: &str, rtype: RecordType) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;

    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);

    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

#[tokio::test]
async fn logging_before_request_returns_continue() {
    use doggy_dns_core::middleware::logging::LoggingMiddleware;
    let mw = LoggingMiddleware;
    let request = make_dns_request("example.com.", RecordType::A);
    let action = mw.before_request(&request).await;
    assert!(matches!(action, MiddlewareAction::Continue));
}

#[tokio::test]
async fn logging_after_request_completes_without_panic() {
    use doggy_dns_core::middleware::logging::LoggingMiddleware;
    let mw = LoggingMiddleware;
    let request = make_dns_request("example.com.", RecordType::A);
    let outcome = RequestOutcome::success("native".to_string(), 1, None, None);
    mw.after_request(&request, 42, &outcome).await;
}
