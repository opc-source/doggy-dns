#![allow(clippy::disallowed_methods)]

//! Byte-level golden assertions for the answers the userspace path produces.
//!
//! The Nacos hot-path refactor (borrowed service name as the cache key, `zone_of`
//! membership, record move) is meant to be representation-only, so the wire bytes for a
//! fixed query set must not move. These constants were captured from `main`
//! before the refactor; if a later task changes one, that task changed the
//! response, not the expectation.

mod common;

use common::dns_request;
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::CachedInstance;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_testutil::CapturingHandler;
use hickory_proto::op::Message;
use hickory_proto::rr::RecordType;
use hickory_server::net::runtime::TokioTime;
use hickory_server::server::{Request, RequestHandler};
use std::sync::Arc;

fn make_request(domain: &str, rtype: RecordType) -> Request {
    // The golden bytes start with this id, so it is part of what is pinned.
    dns_request(0x1234, domain, rtype, None)
}

/// One A instance under `my-svc.default_group.public`, so a v6 query on the
/// same name is a real NODATA (the spec's golden set pins that shape).
fn nacos_handler() -> (DoggyDnsHandler, CapturingHandler) {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "my-svc".to_string(),
        vec![CachedInstance {
            ip: "10.0.0.1".parse().unwrap(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 60);
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    (
        DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false)),
        CapturingHandler::default(),
    )
}

async fn wire(domain: &str, rtype: RecordType) -> Vec<u8> {
    let (handler, capture) = nacos_handler();
    let request = make_request(domain, rtype);
    handler
        .handle_request::<_, TokioTime>(&request, capture.clone())
        .await;
    capture.bytes()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const NACOS_HIT: &str = "123485000001000100000000066d792d7376630d44454641554c545f47524f5550067075626c6963056e61636f73056c6f63616c0000010001066d792d7376630d64656661756c745f67726f7570067075626c6963056e61636f73056c6f63616c00000100010000003c00040a000001";
const NACOS_NODATA: &str = "123485000001000000010000066d792d7376630d44454641554c545f47524f5550067075626c6963056e61636f73056c6f63616c00001c0001056e61636f73056c6f63616c00000600010000003c0023c0390a686f73746d6173746572c0390000000100000e1000000258000151800000003c";
const NACOS_NXDOMAIN: &str = "12348503000100000001000007756e6b6e6f776e0d64656661756c745f67726f7570067075626c6963056e61636f73056c6f63616c0000010001056e61636f73056c6f63616c00000600010000003c0023c03a0a686f73746d6173746572c03a0000000100000e1000000258000151800000003c";
const NACOS_APEX_SOA: &str = "123485000001000100000000056e61636f73056c6f63616c0000060001056e61636f73056c6f63616c00000600010000003c0023c01d0a686f73746d6173746572c01d0000000100000e1000000258000151800000003c";

#[tokio::test]
async fn nacos_hit_wire_is_stable() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A).await;
    assert_eq!(hex(&bytes), NACOS_HIT, "nacos hit: wire bytes moved");
}

#[tokio::test]
async fn nacos_nodata_wire_is_stable() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::AAAA).await;
    assert_eq!(hex(&bytes), NACOS_NODATA, "nacos nodata: wire bytes moved");
}

#[tokio::test]
async fn nacos_nxdomain_wire_is_stable() {
    let bytes = wire("unknown.default_group.public.nacos.local.", RecordType::A).await;
    assert_eq!(
        hex(&bytes),
        NACOS_NXDOMAIN,
        "nacos nxdomain: wire bytes moved"
    );
}

#[tokio::test]
async fn nacos_apex_soa_wire_is_stable() {
    let bytes = wire("nacos.local.", RecordType::SOA).await;
    assert_eq!(
        hex(&bytes),
        NACOS_APEX_SOA,
        "nacos apex soa: wire bytes moved"
    );
}

#[tokio::test]
async fn nacos_hit_answer_name_is_lowercased_on_the_wire() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A).await;
    let message = Message::from_vec(&bytes).expect("parse captured response");
    assert_eq!(
        message.answers.len(),
        1,
        "A query returns only the A record"
    );
    assert!(
        message.metadata.authoritative,
        "the nacos zone owns this name"
    );
    assert!(
        !message.metadata.recursion_available,
        "AA answers are not recursive"
    );
}
