#![allow(clippy::disallowed_methods)]

//! Wire-level query builders shared by the `doggy-dns-core` test targets and
//! the handler benchmark.
//!
//! A `Request` cannot be built from a `Query` alone: hickory hands handlers the
//! datagram it parsed, so a test must encode a whole message first. Each target
//! used to repeat those ten lines, with the message id and the EDNS opt-in
//! drifting between copies — the golden test pins its id into the expected
//! bytes, so the id stays an explicit argument rather than a hidden default.

use hickory_proto::op::{Edns, Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::Request;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

/// Encode one recursion-desired `UDP` query for `domain`/`rtype`.
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
