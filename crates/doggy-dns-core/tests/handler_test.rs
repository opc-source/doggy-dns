#![allow(clippy::disallowed_methods)]

mod common;

use async_trait::async_trait;
use common::dns_request;
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::CachedInstance;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use doggy_dns_testutil::{CapturingHandler, MockZoneHandler};
use hickory_proto::op::{Edns, Message, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordType};
use hickory_server::net::runtime::TokioTime;
use hickory_server::server::{Request, RequestHandler};
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupError, LookupOptions, LookupRecords, ZoneType,
};
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// --- Helpers ---

fn make_dns_request(domain: &str, rtype: RecordType) -> Request {
    dns_request(1234, domain, rtype, None)
}

fn make_edns_dns_request(domain: &str, rtype: RecordType, max_payload: u16) -> Request {
    let mut edns = Edns::new();
    edns.set_max_payload(max_payload);
    dns_request(1234, domain, rtype, Some(edns))
}

/// A middleware that always short-circuits.
struct ShortCircuitMiddleware {
    after_request_count: AtomicU64,
}

impl ShortCircuitMiddleware {
    fn new() -> Self {
        Self {
            after_request_count: AtomicU64::new(0),
        }
    }

    fn after_request_calls(&self) -> u64 {
        self.after_request_count.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Middleware for ShortCircuitMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        MiddlewareAction::ShortCircuit
    }

    async fn after_request(
        &self,
        _request: &Request,
        _duration_ms: u64,
        _outcome: &RequestOutcome,
    ) {
        self.after_request_count.fetch_add(1, Ordering::Relaxed);
    }
}

type MapCatalog = DashMap<String, Vec<CachedInstance>>;

fn make_nacos_authority_with_hit() -> NacosAuthority<MapCatalog> {
    make_nacos_authority_with_ips(&["10.0.0.1"])
}

fn make_nacos_authority_with_ips(ips: &[&str]) -> NacosAuthority<MapCatalog> {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "my-svc".to_string(),
        ips.iter()
            .map(|ip| CachedInstance {
                ip: ip.parse().unwrap(),
                port: 8080,
                healthy: true,
            })
            .collect(),
    );
    NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 60)
}

/// Captures the final `RequestOutcome` so tests can inspect the XDP cache payload.
struct OutcomeCaptureMiddleware {
    outcome: Arc<std::sync::Mutex<Option<RequestOutcome>>>,
}

impl OutcomeCaptureMiddleware {
    fn new() -> Self {
        Self {
            outcome: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn outcome(&self) -> Option<RequestOutcome> {
        self.outcome.lock().unwrap().clone()
    }
}

#[async_trait]
impl Middleware for OutcomeCaptureMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        MiddlewareAction::Continue
    }

    async fn after_request(&self, _request: &Request, _duration_ms: u64, outcome: &RequestOutcome) {
        *self.outcome.lock().unwrap() = Some(outcome.clone());
    }
}

// --- Tests ---

#[tokio::test]
async fn handler_resolves_query_returns_no_error() {
    let authority = make_nacos_authority_with_hit();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
}

#[tokio::test]
async fn handler_all_skip_returns_nxdomain() {
    let chain = AuthorityChain::new(vec![]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("unknown.example.com.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NXDomain));
}

#[tokio::test]
async fn handler_authority_error_returns_servfail() {
    let chain = AuthorityChain::new(vec![(
        "error".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::Primary,
            "error.local.",
            |_, _, _, _| {
                LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
            },
        )),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("anything.example.com.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::ServFail));
}

#[tokio::test]
async fn handler_middleware_short_circuit_returns_refused() {
    let chain = AuthorityChain::new(vec![]);
    let sc_mw = Arc::new(ShortCircuitMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![sc_mw.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        false,
        AnswerRotation::new(false),
    );

    let request = make_dns_request("example.com.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::Refused));
}

#[tokio::test]
async fn handler_after_request_called_on_short_circuit() {
    let chain = AuthorityChain::new(vec![]);
    let sc_mw = Arc::new(ShortCircuitMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![sc_mw.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        false,
        AnswerRotation::new(false),
    );

    let request = make_dns_request("example.com.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    // after_request should still be called even on short-circuit
    assert_eq!(sc_mw.after_request_calls(), 1);
}

#[tokio::test]
async fn handler_no_middlewares_resolves_normally() {
    let authority = make_nacos_authority_with_hit();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
}

fn many_ips() -> Vec<String> {
    (1..=40).map(|i| format!("10.0.0.{i}")).collect()
}

#[tokio::test]
async fn handler_edns_query_caches_small_response() {
    let authority = make_nacos_authority_with_hit();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        true,
        AnswerRotation::new(false),
    );

    let request = make_edns_dns_request(
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        1232,
    );
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
    let outcome = capture.outcome().expect("outcome not captured");
    let bytes = outcome
        .response_bytes
        .expect("small EDNS response should be cached");
    assert!(bytes.len() <= 512, "cached payload exceeds XDP capacity");
    let decoded = Message::from_vec(&bytes).expect("cached bytes must be a valid DNS message");
    assert_eq!(decoded.metadata.response_code, ResponseCode::NoError);
    assert_eq!(decoded.answers.len(), 1);
}

#[tokio::test]
async fn handler_oversized_response_is_not_cached() {
    let ips = many_ips();
    let refs: Vec<&str> = ips.iter().map(String::as_str).collect();
    let authority = make_nacos_authority_with_ips(&refs);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        true,
        AnswerRotation::new(false),
    );

    let request = make_edns_dns_request(
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        1232,
    );
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
    let outcome = capture.outcome().expect("outcome not captured");
    assert!(
        outcome.response_bytes.is_none(),
        "response larger than 512 bytes must not be cached"
    );
}

#[tokio::test]
async fn handler_truncated_response_is_not_cached() {
    let ips = many_ips();
    let refs: Vec<&str> = ips.iter().map(String::as_str).collect();
    let authority = make_nacos_authority_with_ips(&refs);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        true,
        AnswerRotation::new(false),
    );

    let request = make_edns_dns_request(
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        512,
    );
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
    let outcome = capture.outcome().expect("outcome not captured");
    assert!(
        outcome.response_bytes.is_none(),
        "truncated responses must not be cached"
    );
}

#[tokio::test]
async fn handler_authority_nxdomain_maps_to_nxdomain() {
    let chain = AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::Primary,
            "nacos.local.",
            |_, _, _, _| {
                LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
            },
        )),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("missing.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NXDomain));
}

#[tokio::test]
async fn handler_authority_name_exists_returns_nodata() {
    let chain = AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::Primary,
            "nacos.local.",
            |_, _, _, _| LookupControlFlow::Continue(Err(LookupError::NameExists)),
        )),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::AAAA);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
}

#[tokio::test]
async fn handler_authoritative_nxdomain_is_cached_for_xdp() {
    let chain = AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::Primary,
            "nacos.local.",
            |_, _, _, _| {
                LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
            },
        )),
    )]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        true,
        AnswerRotation::new(false),
    );

    let request = make_dns_request("missing.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NXDomain));
    let outcome = capture.outcome().expect("outcome not captured");
    let bytes = outcome
        .response_bytes
        .expect("authoritative NXDOMAIN should be cached");
    let decoded = Message::from_vec(&bytes).expect("cached bytes must be a valid DNS message");
    assert_eq!(decoded.metadata.response_code, ResponseCode::NXDomain);
}

/// The all-skip path builds the response and the XDP payload from two different
/// helpers, so nothing else in this file can see them disagree: one decides
/// what the client gets, the other decides what the map replays for the rest of
/// the TTL. One query, both compared.
#[tokio::test]
async fn all_skip_xdp_payload_matches_the_sent_response() {
    let chain = AuthorityChain::new(vec![]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        true,
        AnswerRotation::new(false),
    );

    let request = make_dns_request("unknown.example.com.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    let sent = mock.message().expect("response captured");
    let outcome = capture.outcome().expect("outcome not captured");
    let bytes = outcome
        .response_bytes
        .expect("all-skip NXDOMAIN should be cached for XDP");
    let cached = Message::from_vec(&bytes).expect("cached bytes must be a valid DNS message");

    assert_eq!(cached.metadata.response_code, ResponseCode::NXDomain);
    assert_eq!(cached.metadata.authoritative, sent.metadata.authoritative);
    assert_eq!(
        cached.metadata.recursion_available,
        sent.metadata.recursion_available
    );
    assert!(
        !cached.metadata.authoritative,
        "no authority claimed the name, so nothing owns the answer"
    );
    // The distinction from an authority's own NXDOMAIN: no SOA to replay.
    assert!(cached.answers.is_empty());
    assert!(cached.authorities.is_empty());
}

fn soa_records(message: &Message) -> Vec<&Record> {
    message
        .authorities
        .iter()
        .filter(|record| record.record_type() == RecordType::SOA)
        .collect()
}

#[tokio::test]
async fn handler_authoritative_answer_sets_aa_and_clears_ra() {
    let authority = make_nacos_authority_with_hit();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    let message = mock.message().expect("response not captured");
    assert!(message.metadata.authoritative, "AA must be set");
    assert!(!message.metadata.recursion_available, "RA must be clear");
    assert_eq!(message.answers.len(), 1);
}

#[tokio::test]
async fn handler_authoritative_nxdomain_sets_aa_and_carries_soa() {
    let authority = make_nacos_authority_with_hit();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("missing.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NXDomain));
    let message = mock.message().expect("response not captured");
    assert!(message.metadata.authoritative, "AA must be set");
    assert!(!message.metadata.recursion_available, "RA must be clear");
    let soa = soa_records(&message);
    assert_eq!(soa.len(), 1, "negative answer must carry one SOA");
    assert_eq!(soa[0].name, Name::from_str("nacos.local.").unwrap());
    assert_eq!(soa[0].ttl, 60);
}

#[tokio::test]
async fn handler_authoritative_nodata_sets_aa_and_carries_soa() {
    let authority = make_nacos_authority_with_ips(&["10.0.0.1"]);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::AAAA);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
    let message = mock.message().expect("response not captured");
    assert!(message.metadata.authoritative, "AA must be set");
    assert!(!message.metadata.recursion_available, "RA must be clear");
    assert!(message.answers.is_empty());
    assert_eq!(soa_records(&message).len(), 1);
}

#[tokio::test]
async fn handler_external_answer_sets_ra_and_clears_aa() {
    let chain = AuthorityChain::new(vec![(
        "external".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::External,
            ".",
            |name, _, _, _| {
                let record = Record::from_rdata(
                    Name::from(name.clone()),
                    60,
                    RData::A(A(Ipv4Addr::new(203, 0, 113, 7))),
                );
                LookupControlFlow::Continue(Ok(AuthLookup::answers(
                    LookupRecords::Section(vec![record]),
                    None,
                )))
            },
        )),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("www.example.com.", RecordType::A);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    let message = mock.message().expect("response not captured");
    assert!(!message.metadata.authoritative, "AA must be clear");
    assert!(message.metadata.recursion_available, "RA must be set");
    assert!(soa_records(&message).is_empty());
}

#[tokio::test]
async fn handler_apex_soa_query_returns_authoritative_soa_answer() {
    let authority = make_nacos_authority_with_hit();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(false));

    let request = make_dns_request("nacos.local.", RecordType::SOA);
    let mock = CapturingHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
    let message = mock.message().expect("response not captured");
    assert!(message.metadata.authoritative);
    assert_eq!(message.answers.len(), 1);
    assert_eq!(message.answers[0].record_type(), RecordType::SOA);
}

// --- Answer rotation ---

fn instance(octet: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 0, 0, octet)
}

/// The A-record answers of a message, in the order they went out.
fn answer_ipv4s(message: &Message) -> Vec<Ipv4Addr> {
    message
        .answers
        .iter()
        .map(|record| match &record.data {
            RData::A(address) => address.0,
            other => panic!("expected an A record, got {other:?}"),
        })
        .collect()
}

/// One A query against `handler`; the handler is shared, not rebuilt per query.
async fn ask(handler: &DoggyDnsHandler, domain: &str) -> Message {
    let request = make_dns_request(domain, RecordType::A);
    let mock = CapturingHandler::new();
    handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;
    mock.message().expect("response not captured")
}

/// A three-instance `nacos` service behind one handler, with the outcome
/// capture that carries the XDP cache payload when `xdp_enabled` is set.
///
/// The handler must stay shared across queries: the rotation cursor lives on
/// it, so a fresh handler per query would restart the cycle and prove nothing.
fn rotating_handler(
    rotation: AnswerRotation,
    xdp_enabled: bool,
) -> (DoggyDnsHandler, Arc<OutcomeCaptureMiddleware>) {
    let authority = make_nacos_authority_with_ips(&["10.0.0.1", "10.0.0.2", "10.0.0.3"]);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        xdp_enabled,
        rotation,
    );
    (handler, capture)
}

const THREE_INSTANCE_SERVICE: &str = "my-svc.DEFAULT_GROUP.public.nacos.local.";

#[tokio::test]
async fn rotate_answers_cycles_the_answer_sequence() {
    let (handler, _capture) = rotating_handler(AnswerRotation::new(true), false);

    let mut sequences = Vec::new();
    for _ in 0..4 {
        let message = ask(&handler, THREE_INSTANCE_SERVICE).await;
        assert_eq!(message.answers.len(), 3, "the service has three instances");
        sequences.push(answer_ipv4s(&message));
    }

    assert_eq!(
        sequences,
        [
            vec![instance(1), instance(2), instance(3)],
            vec![instance(2), instance(3), instance(1)],
            vec![instance(3), instance(1), instance(2)],
            vec![instance(1), instance(2), instance(3)],
        ],
        "the whole sequence must walk the instance list and wrap, not just the first record"
    );
}

#[tokio::test]
async fn disabled_rotate_answers_keeps_the_authority_order() {
    let (handler, _capture) = rotating_handler(AnswerRotation::new(false), false);

    for _ in 0..4 {
        let message = ask(&handler, THREE_INSTANCE_SERVICE).await;
        assert_eq!(
            answer_ipv4s(&message),
            [instance(1), instance(2), instance(3)],
            "rotation off must repeat the authority's own order"
        );
    }
}

#[tokio::test]
async fn rotated_answers_reach_the_xdp_cache_payload() {
    // The payload is what the kernel replays on every hit, so it has to be
    // serialised from the rotated records. If it were built from the authority's
    // own order instead, each hit would freeze one arbitrary order and the
    // response-only tests above would stay green.
    let (handler, capture) = rotating_handler(AnswerRotation::new(true), true);

    let mut sent = Vec::new();
    let mut cached = Vec::new();
    for _ in 0..2 {
        let message = ask(&handler, THREE_INSTANCE_SERVICE).await;
        let outcome = capture.outcome().expect("outcome not captured");
        let bytes = outcome
            .response_bytes
            .expect("a three-record answer fits the XDP cache");
        assert!(bytes.len() <= 512, "cached payload exceeds XDP capacity");
        let decoded = Message::from_vec(&bytes).expect("cached bytes must be a valid DNS message");
        assert_eq!(decoded.answers.len(), 3, "the cached reply lost records");
        sent.push(answer_ipv4s(&message));
        cached.push(answer_ipv4s(&decoded));
    }

    assert_eq!(sent, cached, "the cached payload must carry the order sent");
    assert_eq!(
        cached,
        [
            vec![instance(1), instance(2), instance(3)],
            vec![instance(2), instance(3), instance(1)],
        ],
        "the cached payload must advance with the response, not repeat one frozen order"
    );
}

#[tokio::test]
async fn rotate_answers_cycles_the_cloned_record_path() {
    // Rotation lives at the one choke point every record path passes through, so
    // the clone-collect path used by `native`/`forward` answers cycles too.
    //
    // The fake authority keeps its records behind an `Arc` rather than an owned
    // `Vec`: the `LookupRecords::Section` arm of `take_answer_records` does not
    // match, so the records are collected by clone — the same arm
    // `AuthLookup::Resolved`, which `native` and `forward` return, falls through to.
    let chain = AuthorityChain::new(vec![(
        "external".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::External,
            ".",
            |name, _, _, _| {
                let answer_name = Name::from(name.clone());
                let mut record_set =
                    hickory_proto::rr::RecordSet::new(answer_name.clone(), RecordType::A, 0);
                record_set.set_records(vec![
                    Record::from_rdata(
                        answer_name.clone(),
                        60,
                        RData::A(A(Ipv4Addr::new(203, 0, 113, 1))),
                    ),
                    Record::from_rdata(answer_name, 60, RData::A(A(Ipv4Addr::new(203, 0, 113, 2)))),
                ]);
                LookupControlFlow::Continue(Ok(AuthLookup::answers(
                    LookupRecords::new(LookupOptions::default(), Arc::new(record_set)),
                    None,
                )))
            },
        )),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false, AnswerRotation::new(true));

    let mut sequences = Vec::new();
    for _ in 0..3 {
        let message = ask(&handler, "two-a.example.org.").await;
        assert_eq!(
            message.answers.len(),
            2,
            "the fake authority has two records"
        );
        sequences.push(answer_ipv4s(&message));
    }

    assert_eq!(
        sequences,
        [
            vec![Ipv4Addr::new(203, 0, 113, 1), Ipv4Addr::new(203, 0, 113, 2)],
            vec![Ipv4Addr::new(203, 0, 113, 2), Ipv4Addr::new(203, 0, 113, 1)],
            vec![Ipv4Addr::new(203, 0, 113, 1), Ipv4Addr::new(203, 0, 113, 2)],
        ],
        "the clone-collect path must rotate like the move path"
    );
}
