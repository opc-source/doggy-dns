#![allow(clippy::disallowed_methods)]

use doggy_dns_core::middleware::metrics::MetricsMiddleware;
use doggy_dns_plugin::{Middleware, MiddlewareAction};
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::Request;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

fn make_dns_request(domain: &str, rtype: RecordType) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.set_recursion_desired(true);

    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);

    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

#[test]
fn metrics_counter_starts_at_zero() {
    let metrics = MetricsMiddleware::new();
    assert_eq!(metrics.total_queries(), 0);
}

#[tokio::test]
async fn metrics_before_request_increments_counter() {
    let metrics = MetricsMiddleware::new();
    let request = make_dns_request("example.com.", RecordType::A);
    metrics.before_request(&request).await;
    assert_eq!(metrics.total_queries(), 1);
}

#[tokio::test]
async fn metrics_multiple_calls_increment_correctly() {
    let metrics = MetricsMiddleware::new();
    let request = make_dns_request("example.com.", RecordType::A);
    for _ in 0..5 {
        metrics.before_request(&request).await;
    }
    assert_eq!(metrics.total_queries(), 5);
}

#[tokio::test]
async fn metrics_before_request_returns_continue() {
    let metrics = MetricsMiddleware::new();
    let request = make_dns_request("example.com.", RecordType::A);
    let action = metrics.before_request(&request).await;
    assert!(matches!(action, MiddlewareAction::Continue));
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
    mw.after_request(&request, 42).await;
}
