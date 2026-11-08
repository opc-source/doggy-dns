#![allow(clippy::disallowed_methods)]

//! Pins the one rule [`search_via_lookup`] implements: a search answers from the
//! handler's own `lookup` for the request's name and type, and attaches no TSIG
//! context. Nothing else in the suite calls `ZoneHandler::search`.

use async_trait::async_trait;
use doggy_dns_plugin::zone_handler::search_via_lookup;
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, LookupRecords, ZoneHandler, ZoneType,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Records every `lookup` it receives and breaks with no answers.
struct RecordingHandler {
    /// What each `lookup` received: name, type, whether request info was
    /// attached, and the forwarded options' DO bit.
    seen: Arc<Mutex<Vec<(LowerName, RecordType, bool, bool)>>>,
}

#[async_trait]
impl ZoneHandler for RecordingHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        static ORIGIN: std::sync::LazyLock<LowerName> =
            std::sync::LazyLock::new(|| LowerName::from(Name::from_str("search.local.").unwrap()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        request_info: Option<&RequestInfo<'_>>,
        lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        self.seen.lock().unwrap().push((
            name.clone(),
            rtype,
            request_info.is_some(),
            lookup_options.dnssec_ok,
        ));
        LookupControlFlow::Break(Ok(AuthLookup::answers(
            LookupRecords::Section(vec![]),
            None,
        )))
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

fn dns_request(domain: &str, rtype: RecordType) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;

    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);

    encode(message)
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
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = RecordingHandler {
        seen: Arc::clone(&seen),
    };
    let request = dns_request("www.search.local.", RecordType::AAAA);
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
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = RecordingHandler {
        seen: Arc::clone(&seen),
    };
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
