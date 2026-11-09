#![allow(clippy::disallowed_methods)]

//! Pins the one rule [`search_via_lookup`] implements: a search answers from the
//! handler's own `lookup` for the request's name and type, and attaches no TSIG
//! context. Nothing else in the suite calls `ZoneHandler::search`.

use doggy_dns_plugin::zone_handler::search_via_lookup;
use doggy_dns_testutil::{MockZoneHandler, dns_request};
use hickory_proto::op::{Message, MessageType, OpCode};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::Request;
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupOptions, LookupRecords, ZoneType,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// What each `lookup` received: name, type, whether request info was attached,
/// and the forwarded options' DO bit.
type Seen = Arc<Mutex<Vec<(LowerName, RecordType, bool, bool)>>>;

/// A handler that records every `lookup` it receives and breaks with no answers.
fn recording_handler(seen: Seen) -> MockZoneHandler {
    MockZoneHandler::new(
        ZoneType::Primary,
        "search.local.",
        move |name, rtype, info, options| {
            seen.lock()
                .unwrap()
                .push((name.clone(), rtype, info.is_some(), options.dnssec_ok));
            LookupControlFlow::Break(Ok(AuthLookup::answers(
                LookupRecords::Section(vec![]),
                None,
            )))
        },
    )
}

fn dns_request_without_question() -> Request {
    encode(Message::new(1234, MessageType::Query, OpCode::Query))
}

fn encode(message: Message) -> Request {
    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

#[tokio::test]
async fn search_answers_from_lookup_for_the_requests_name_and_type() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let handler = recording_handler(Arc::clone(&seen));
    let request = dns_request(1234, "www.search.local.", RecordType::AAAA, None);
    let mut options = LookupOptions::default();
    options.dnssec_ok = true;

    let (flow, tsig) = search_via_lookup(&handler, &request, options).await;

    assert!(
        matches!(flow, LookupControlFlow::Break(Ok(_))),
        "the search must return the handler's own outcome"
    );
    assert!(tsig.is_none(), "a search attaches no TSIG context");

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        &[(
            LowerName::from(Name::from_str("www.search.local.").unwrap()),
            RecordType::AAAA,
            true,
            true,
        )],
        "lookup must receive the request's name, type, options and request info"
    );
}

#[tokio::test]
async fn search_skips_a_request_without_exactly_one_question() {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let handler = recording_handler(Arc::clone(&seen));
    let request = dns_request_without_question();

    let (flow, tsig) = search_via_lookup(&handler, &request, LookupOptions::default()).await;

    assert!(
        matches!(flow, LookupControlFlow::Skip),
        "a request without exactly one question cannot be searched"
    );
    assert!(tsig.is_none());
    assert!(
        seen.lock().unwrap().is_empty(),
        "lookup must not run for a request the search cannot read"
    );
}
