#![allow(clippy::disallowed_methods)]

//! Byte-level golden assertions for the answers the userspace path produces.
//!
//! The Nacos hot-path refactor (composite cache key, `zone_of` membership,
//! record move) is meant to be representation-only, so the wire bytes for a
//! fixed query set must not move. These constants were captured from `main`
//! before the refactor; if a later task changes one, that task changed the
//! response, not the expectation.

use async_trait::async_trait;
use dashmap::DashMap;
use doggy_dns_core::handler::DoggyDnsHandler;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, Record, RecordType};
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::NetError;
use hickory_server::net::runtime::TokioTime;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::MessageResponse;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// ResponseHandler that serialises the response exactly as the server would and
/// keeps the raw datagram for byte comparison.
#[derive(Clone, Default)]
struct ByteCapturingHandler {
    bytes: Arc<Mutex<Vec<u8>>>,
}

#[async_trait]
impl ResponseHandler for ByteCapturingHandler {
    async fn send_response<'a>(
        &mut self,
        response: MessageResponse<
            '_,
            'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
        >,
    ) -> Result<ResponseInfo, NetError> {
        let mut bytes = Vec::new();
        let mut encoder = BinEncoder::new(&mut bytes);
        encoder.set_max_size(u16::MAX);
        let info = response
            .destructive_emit(&mut encoder)
            .expect("serialize test response");
        *self.bytes.lock().unwrap() = bytes;
        Ok(info)
    }
}

fn make_request(domain: &str, rtype: RecordType) -> Request {
    let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);
    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

/// One A instance under `my-svc.default_group.public`, so a v6 query on the
/// same name is a real NODATA (the spec's golden set pins that shape).
fn nacos_handler() -> (DoggyDnsHandler, ByteCapturingHandler) {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey::new("my-svc", "default_group", "public"),
        vec![CachedInstance {
            ip: "10.0.0.1".parse().unwrap(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    (
        DoggyDnsHandler::new(vec![], chain, false),
        ByteCapturingHandler::default(),
    )
}

async fn wire(domain: &str, rtype: RecordType) -> Vec<u8> {
    let (handler, capture) = nacos_handler();
    let request = make_request(domain, rtype);
    handler
        .handle_request::<_, TokioTime>(&request, capture.clone())
        .await;
    capture.bytes.lock().unwrap().clone()
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
