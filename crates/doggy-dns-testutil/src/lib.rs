#![allow(clippy::disallowed_methods)]

//! Shared test scaffolding for the doggy-dns test targets and benchmarks.
//!
//! Dev-only (`publish = false`): nothing in a product crate depends on this, so
//! the helpers may `unwrap()` exactly like the test files they came from.
//!
//! The pieces here used to be copied per test target: the wire-level query
//! builder (five copies), the response handler that serialises what the server
//! would send (`MockResponseHandler`, `ByteCapturingHandler`,
//! `MockResponseHandler` again as a no-op), and the ~50-line `ZoneHandler` mocks
//! (`ErrorZoneHandler`, `LookupErrorZoneHandler`, `BreakZoneHandler`,
//! `ExternalAnswerHandler`, ...).

use async_trait::async_trait;
use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{LowerName, Name, Record, RecordType};
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::NetError;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestInfo, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, MessageResponse, ZoneHandler,
    ZoneType,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Encode one recursion-desired `UDP` query for `domain`/`rtype`.
///
/// A `Request` cannot be built from a `Query` alone: hickory hands handlers the
/// datagram it parsed, so a test must encode a whole message first. Each target
/// used to repeat those ten lines, with the message id and the EDNS opt-in
/// drifting between copies — the golden test pins its id into the expected
/// bytes, so the id stays an explicit argument rather than a hidden default.
pub fn dns_request(id: u16, domain: &str, rtype: RecordType, edns: Option<Edns>) -> Request {
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;

    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);
    if let Some(edns) = edns {
        message.set_edns(edns);
    }

    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

/// A `ResponseHandler` that serialises the response exactly as the server would.
///
/// It keeps the raw datagram for byte comparison and the reparsed `Message` for
/// structural assertions. The message is parsed from the emitted bytes, never
/// re-serialised from the response object, so a caller asserting on it sees the
/// same thing a client would.
#[derive(Clone, Default)]
pub struct CapturingHandler {
    bytes: Arc<Mutex<Vec<u8>>>,
    message: Arc<Mutex<Option<Message>>>,
}

impl CapturingHandler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The last emitted datagram.
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// The last emitted datagram, reparsed.
    pub fn message(&self) -> Option<Message> {
        self.message.lock().unwrap().clone()
    }

    /// The response code of the last emitted datagram, if anything was sent.
    pub fn response_code(&self) -> Option<ResponseCode> {
        self.message().map(|message| message.metadata.response_code)
    }
}

#[async_trait]
impl ResponseHandler for CapturingHandler {
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
        let message = Message::from_vec(&bytes).expect("parse serialized test response");
        *self.bytes.lock().unwrap() = bytes;
        *self.message.lock().unwrap() = Some(message);
        Ok(info)
    }
}

/// The lookup decision a [`MockZoneHandler`] makes for one query.
///
/// The full `lookup` argument list is forwarded, so a test that observes the
/// request info or the lookup options needs no `ZoneHandler` boilerplate of its
/// own.
type LookupFn = dyn for<'a, 'b, 'c> Fn(
        &'a LowerName,
        RecordType,
        Option<&'b RequestInfo<'c>>,
        LookupOptions,
    ) -> LookupControlFlow<AuthLookup>
    + Send
    + Sync;

/// A `ZoneHandler` backed by a closure.
///
/// `search` and `nsec_records` always skip; every test that needed a handler
/// mock only ever varied the `lookup` outcome and the zone identity, which is
/// what the constructor takes.
pub struct MockZoneHandler {
    zone_type: ZoneType,
    origin: LowerName,
    behavior: Box<LookupFn>,
}

impl MockZoneHandler {
    /// A handler in `zone_type` zone with `origin`, answering through `behavior`.
    pub fn new(
        zone_type: ZoneType,
        origin: &str,
        behavior: impl for<'a, 'b, 'c> Fn(
            &'a LowerName,
            RecordType,
            Option<&'b RequestInfo<'c>>,
            LookupOptions,
        ) -> LookupControlFlow<AuthLookup>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            zone_type,
            origin: LowerName::from(Name::from_str(origin).expect("valid zone origin")),
            behavior: Box::new(behavior),
        }
    }
}

#[async_trait]
impl ZoneHandler for MockZoneHandler {
    fn zone_type(&self) -> ZoneType {
        self.zone_type
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        request_info: Option<&RequestInfo<'_>>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        (self.behavior)(name, rtype, request_info, lookup_options)
    }

    async fn search(
        &self,
        _request: &Request,
        _lookup_options: LookupOptions,
    ) -> (
        LookupControlFlow<AuthLookup>,
        Option<hickory_proto::rr::TSigResponseContext>,
    ) {
        (LookupControlFlow::Skip, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}
