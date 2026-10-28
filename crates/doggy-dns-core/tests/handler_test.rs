#![allow(clippy::disallowed_methods)]

use async_trait::async_trait;
use dashmap::DashMap;
use doggy_dns_core::handler::DoggyDnsHandler;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::{
    Edns, Header, HeaderCounts, Message, MessageType, OpCode, Query, ResponseCode,
};
use hickory_proto::rr::{Name, Record, RecordType};
use hickory_server::net::NetError;
use hickory_server::net::runtime::TokioTime;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupError, LookupOptions, MessageResponse, ZoneHandler,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// --- Helpers ---

fn make_dns_request(domain: &str, rtype: RecordType) -> Request {
    build_request(domain, rtype, None)
}

fn make_edns_dns_request(domain: &str, rtype: RecordType, max_payload: u16) -> Request {
    let mut edns = Edns::new();
    edns.set_max_payload(max_payload);
    build_request(domain, rtype, Some(edns))
}

fn build_request(domain: &str, rtype: RecordType, edns: Option<Edns>) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
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

/// Mock ResponseHandler that captures the response header info.
#[derive(Clone)]
struct MockResponseHandler {
    last_response_code: Arc<std::sync::Mutex<Option<ResponseCode>>>,
}

impl MockResponseHandler {
    fn new() -> Self {
        Self {
            last_response_code: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn response_code(&self) -> Option<ResponseCode> {
        *self.last_response_code.lock().unwrap()
    }
}

#[async_trait]
impl ResponseHandler for MockResponseHandler {
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
        let metadata = *response.metadata();
        *self.last_response_code.lock().unwrap() = Some(metadata.response_code);
        Ok(ResponseInfo::from(Header {
            metadata,
            counts: HeaderCounts::default(),
        }))
    }
}

/// A ZoneHandler that always returns Continue(Err(...)) for error path testing.
struct ErrorZoneHandler;

#[async_trait]
impl ZoneHandler for ErrorZoneHandler {
    fn zone_type(&self) -> hickory_server::zone_handler::ZoneType {
        hickory_server::zone_handler::ZoneType::Primary
    }

    fn axfr_policy(&self) -> hickory_server::zone_handler::AxfrPolicy {
        hickory_server::zone_handler::AxfrPolicy::Deny
    }

    fn origin(&self) -> &hickory_proto::rr::LowerName {
        static ORIGIN: std::sync::LazyLock<hickory_proto::rr::LowerName> =
            std::sync::LazyLock::new(|| {
                hickory_proto::rr::LowerName::from(Name::from_str("error.local.").unwrap())
            });
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &hickory_proto::rr::LowerName,
        _rtype: RecordType,
        _request_info: Option<&hickory_server::server::RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Continue(Err(
            hickory_server::zone_handler::LookupError::ResponseCode(ResponseCode::ServFail),
        ))
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
        _name: &hickory_proto::rr::LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

/// A ZoneHandler that returns a specific authoritative negative result.
struct LookupErrorZoneHandler {
    kind: ExpectedLookupError,
}

#[derive(Clone, Copy)]
enum ExpectedLookupError {
    NxDomain,
    NameExists,
}

impl LookupErrorZoneHandler {
    fn new(kind: ExpectedLookupError) -> Self {
        Self { kind }
    }
}

#[async_trait]
impl ZoneHandler for LookupErrorZoneHandler {
    fn zone_type(&self) -> hickory_server::zone_handler::ZoneType {
        hickory_server::zone_handler::ZoneType::Primary
    }

    fn axfr_policy(&self) -> hickory_server::zone_handler::AxfrPolicy {
        hickory_server::zone_handler::AxfrPolicy::Deny
    }

    fn origin(&self) -> &hickory_proto::rr::LowerName {
        static ORIGIN: std::sync::LazyLock<hickory_proto::rr::LowerName> =
            std::sync::LazyLock::new(|| {
                hickory_proto::rr::LowerName::from(Name::from_str("nacos.local.").unwrap())
            });
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &hickory_proto::rr::LowerName,
        _rtype: RecordType,
        _request_info: Option<&hickory_server::server::RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let error = match self.kind {
            ExpectedLookupError::NxDomain => LookupError::ResponseCode(ResponseCode::NXDomain),
            ExpectedLookupError::NameExists => LookupError::NameExists,
        };
        LookupControlFlow::Continue(Err(error))
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
        _name: &hickory_proto::rr::LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
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

fn make_nacos_authority_with_hit() -> NacosAuthority {
    make_nacos_authority_with_ips(&["10.0.0.1"])
}

fn make_nacos_authority_with_ips(ips: &[&str]) -> NacosAuthority {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "my-svc".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        ips.iter()
            .map(|ip| CachedInstance {
                ip: (*ip).to_string(),
                port: 8080,
                healthy: true,
            })
            .collect(),
    );
    NacosAuthority::new(cache, "nacos.local", 60)
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
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false);

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = MockResponseHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
}

#[tokio::test]
async fn handler_all_skip_returns_nxdomain() {
    let chain = AuthorityChain::new(vec![]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false);

    let request = make_dns_request("unknown.example.com.", RecordType::A);
    let mock = MockResponseHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NXDomain));
}

#[tokio::test]
async fn handler_authority_error_returns_servfail() {
    let chain = AuthorityChain::new(vec![("error".to_string(), Arc::new(ErrorZoneHandler))]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false);

    let request = make_dns_request("anything.example.com.", RecordType::A);
    let mock = MockResponseHandler::new();
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
    );

    let request = make_dns_request("example.com.", RecordType::A);
    let mock = MockResponseHandler::new();
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
    );

    let request = make_dns_request("example.com.", RecordType::A);
    let mock = MockResponseHandler::new();
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
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false);

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = MockResponseHandler::new();
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
    );

    let request = make_edns_dns_request(
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        1232,
    );
    let mock = MockResponseHandler::new();
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
    );

    let request = make_edns_dns_request(
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        1232,
    );
    let mock = MockResponseHandler::new();
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
    );

    let request = make_edns_dns_request(
        "my-svc.DEFAULT_GROUP.public.nacos.local.",
        RecordType::A,
        512,
    );
    let mock = MockResponseHandler::new();
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
        Arc::new(LookupErrorZoneHandler::new(ExpectedLookupError::NxDomain)),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false);

    let request = make_dns_request("missing.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = MockResponseHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NXDomain));
}

#[tokio::test]
async fn handler_authority_name_exists_returns_nodata() {
    let chain = AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(LookupErrorZoneHandler::new(ExpectedLookupError::NameExists)),
    )]);
    let handler = DoggyDnsHandler::new(vec![], Arc::new(chain), false);

    let request = make_dns_request("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::AAAA);
    let mock = MockResponseHandler::new();
    let _info = handler
        .handle_request::<_, TokioTime>(&request, mock.clone())
        .await;

    assert_eq!(mock.response_code(), Some(ResponseCode::NoError));
}

#[tokio::test]
async fn handler_authoritative_nxdomain_is_cached_for_xdp() {
    let chain = AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(LookupErrorZoneHandler::new(ExpectedLookupError::NxDomain)),
    )]);
    let capture = Arc::new(OutcomeCaptureMiddleware::new());
    let handler = DoggyDnsHandler::new(
        vec![capture.clone() as Arc<dyn Middleware>],
        Arc::new(chain),
        true,
    );

    let request = make_dns_request("missing.DEFAULT_GROUP.public.nacos.local.", RecordType::A);
    let mock = MockResponseHandler::new();
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
