#![allow(clippy::disallowed_methods)]

mod common;

use common::dns_request;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::rr::RecordType;
use hickory_server::server::Request;

fn make_dns_request(domain: &str, rtype: RecordType) -> Request {
    dns_request(1234, domain, rtype, None)
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
    let outcome = RequestOutcome::success("native".into(), 1, None, None);
    mw.after_request(&request, 42, &outcome).await;
}
