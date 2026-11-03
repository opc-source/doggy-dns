#![allow(clippy::disallowed_methods)]

//! A config reload reaches the query path through two swaps on one shared
//! handler: `swap_runtime` (middlewares + chain) and `set_rotation`. These
//! tests pin what a reload caller needs to hold true — a request keeps the
//! snapshot it entered with, and toggling rotation does not restart the walk.

use async_trait::async_trait;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler, SharedHandler};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::rdata::A;
use hickory_proto::rr::{Name, RData, Record, RecordSet, RecordType};
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::runtime::TokioTime;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, LookupRecords, MessageResponse,
    ZoneHandler, ZoneType,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// --- Stub authority ---

/// Answers every A query in `zone` with one address, optionally after a delay.
/// The delay is what lets a swap land while a query is still in flight.
struct StubAuthority {
    address: Ipv4Addr,
    delay: Option<std::time::Duration>,
}

impl StubAuthority {
    fn new(octet: u8) -> Self {
        Self {
            address: Ipv4Addr::new(10, 0, 0, octet),
            delay: None,
        }
    }

    fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

#[async_trait]
impl ZoneHandler for StubAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::External
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &hickory_proto::rr::LowerName {
        static ORIGIN: std::sync::LazyLock<hickory_proto::rr::LowerName> =
            std::sync::LazyLock::new(|| hickory_proto::rr::LowerName::from(Name::root()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        name: &hickory_proto::rr::LowerName,
        _rtype: RecordType,
        _request_info: Option<&hickory_server::server::RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let answer_name = Name::from(name.clone());
        let mut records = RecordSet::new(answer_name.clone(), RecordType::A, 60);
        records.set_records(vec![Record::from_rdata(
            answer_name,
            60,
            RData::A(A(self.address)),
        )]);
        LookupControlFlow::Continue(Ok(AuthLookup::answers(
            LookupRecords::new(LookupOptions::default(), Arc::new(records)),
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
        _name: &hickory_proto::rr::LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

// --- Stub middleware ---

/// Counts how many requests reached it, so a swap of the middleware set is
/// observable from the outside.
struct CountingMiddleware {
    before: Arc<AtomicUsize>,
}

#[async_trait]
impl Middleware for CountingMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        self.before.fetch_add(1, Ordering::Relaxed);
        MiddlewareAction::Continue
    }

    async fn after_request(
        &self,
        _request: &Request,
        _duration_ms: u64,
        _outcome: &RequestOutcome,
    ) {
    }
}

// --- Harness ---

#[derive(Clone)]
struct CapturingResponseHandler {
    message: Arc<std::sync::Mutex<Option<Message>>>,
}

impl CapturingResponseHandler {
    fn new() -> Self {
        Self {
            message: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn answer_ipv4(&self) -> Ipv4Addr {
        let message = self
            .message
            .lock()
            .unwrap()
            .clone()
            .expect("response not captured");
        match &message.answers.first().expect("one answer record").data {
            RData::A(address) => address.0,
            other => panic!("expected an A record, got {other:?}"),
        }
    }
}

#[async_trait]
impl ResponseHandler for CapturingResponseHandler {
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
    ) -> Result<ResponseInfo, hickory_server::net::NetError> {
        let mut bytes = Vec::new();
        let mut encoder = BinEncoder::new(&mut bytes);
        encoder.set_max_size(u16::MAX);
        let info = response
            .destructive_emit(&mut encoder)
            .expect("serialize test response");
        *self.message.lock().unwrap() =
            Some(Message::from_vec(&bytes).expect("parse serialized response"));
        Ok(info)
    }
}

fn query(name: &str) -> Request {
    let mut message = Message::new(4321, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    let mut query = Query::new();
    query.set_name(Name::from_str(name).unwrap());
    query.set_query_type(RecordType::A);
    message.add_query(query);
    let bytes = message.to_vec().unwrap();
    Request::from_bytes(
        bytes,
        SocketAddr::from((Ipv4Addr::LOCALHOST, 12345)),
        Protocol::Udp,
    )
    .unwrap()
}

async fn ask(handler: &DoggyDnsHandler, name: &str) -> CapturingResponseHandler {
    let capture = CapturingResponseHandler::new();
    handler
        .handle_request::<_, TokioTime>(&query(name), capture.clone())
        .await;
    capture
}

fn chain(octet: u8) -> Arc<AuthorityChain> {
    Arc::new(AuthorityChain::new(vec![(
        format!("stub[{octet}]"),
        Arc::new(StubAuthority::new(octet)) as Arc<dyn ZoneHandler>,
    )]))
}

const NAME: &str = "svc.reload.test.";

#[tokio::test]
async fn swap_runtime_changes_the_chain_and_the_middleware_set() {
    let before = Arc::new(AtomicUsize::new(0));
    let counting = Arc::new(CountingMiddleware {
        before: Arc::clone(&before),
    });
    let handler = DoggyDnsHandler::new(
        vec![counting as Arc<dyn Middleware>],
        chain(1),
        false,
        AnswerRotation::new(false),
    );

    assert_eq!(
        ask(&handler, NAME).await.answer_ipv4(),
        Ipv4Addr::new(10, 0, 0, 1)
    );
    assert_eq!(before.load(Ordering::Relaxed), 1);

    // What a reload does: a new chain and a new (empty) middleware set.
    handler.swap_runtime(vec![], chain(2));

    assert_eq!(
        ask(&handler, NAME).await.answer_ipv4(),
        Ipv4Addr::new(10, 0, 0, 2)
    );
    assert_eq!(
        before.load(Ordering::Relaxed),
        1,
        "the retired middleware must not see the queries after the swap"
    );
    assert!(handler.middlewares().is_empty());
}

#[tokio::test]
async fn in_flight_query_keeps_the_chain_it_entered_with() {
    // The slow authority answers after the swap: the request must still be
    // served by the old chain, because `handle_request` took one snapshot at
    // entry and holds it for the whole exchange.
    let slow: Arc<dyn ZoneHandler> =
        Arc::new(StubAuthority::new(9).with_delay(std::time::Duration::from_millis(50)));
    let handler = Arc::new(DoggyDnsHandler::new(
        vec![],
        Arc::new(AuthorityChain::new(vec![("slow".to_string(), slow)])),
        false,
        AnswerRotation::new(false),
    ));

    let in_flight = {
        let handler = Arc::clone(&handler);
        tokio::spawn(async move {
            let capture = CapturingResponseHandler::new();
            SharedHandler(handler)
                .handle_request::<_, TokioTime>(&query(NAME), capture.clone())
                .await;
            capture
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    handler.swap_runtime(vec![], chain(8));

    assert_eq!(
        in_flight.await.expect("query task").answer_ipv4(),
        Ipv4Addr::new(10, 0, 0, 9),
        "a query that entered before the swap must be answered by the old chain"
    );
    // And the new chain is in force for what comes after.
    assert_eq!(
        ask(&handler, NAME).await.answer_ipv4(),
        Ipv4Addr::new(10, 0, 0, 8)
    );
}

#[tokio::test]
async fn set_rotation_leaves_the_cursor_where_it_was() {
    let authority = Arc::new(ThreeInstanceAuthority);
    let handler = DoggyDnsHandler::new(
        vec![],
        Arc::new(AuthorityChain::new(vec![(
            "three".to_string(),
            authority as Arc<dyn ZoneHandler>,
        )])),
        false,
        AnswerRotation::new(true),
    );

    let mut on = Vec::new();
    for _ in 0..4 {
        on.push(first_octets(&ask_multi(&handler, NAME).await));
    }
    assert_eq!(
        on,
        [
            vec![100, 101, 102],
            vec![101, 102, 100],
            vec![102, 100, 101],
            vec![100, 101, 102],
        ],
        "the four responses before the toggle walk the instance list and wrap"
    );

    // Off: the authority's own order, and no advance at all.
    handler.set_rotation(false);
    assert!(!handler.rotation_enabled());
    let off = first_octets(&ask_multi(&handler, NAME).await);
    handler.set_rotation(true);
    let after = first_octets(&ask_multi(&handler, NAME).await);

    assert_eq!(
        off,
        vec![100, 101, 102],
        "rotation off repeats the authority order"
    );
    assert_eq!(
        after,
        vec![101, 102, 100],
        "the cursor had reached 4, so the next step is 4 % 3 = 1; a cursor reset \
         by the toggle would have produced 100 first again"
    );
}

/// Three records so a rotation step is visible in the answer order.
struct ThreeInstanceAuthority;

#[async_trait]
impl ZoneHandler for ThreeInstanceAuthority {
    fn zone_type(&self) -> ZoneType {
        ZoneType::External
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &hickory_proto::rr::LowerName {
        static ORIGIN: std::sync::LazyLock<hickory_proto::rr::LowerName> =
            std::sync::LazyLock::new(|| hickory_proto::rr::LowerName::from(Name::root()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        name: &hickory_proto::rr::LowerName,
        _rtype: RecordType,
        _request_info: Option<&hickory_server::server::RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        let answer_name = Name::from(name.clone());
        let mut records = RecordSet::new(answer_name.clone(), RecordType::A, 60);
        records.set_records(
            [100u8, 101, 102]
                .iter()
                .map(|octet| {
                    Record::from_rdata(
                        answer_name.clone(),
                        60,
                        RData::A(A(Ipv4Addr::new(10, 0, 0, *octet))),
                    )
                })
                .collect(),
        );
        LookupControlFlow::Continue(Ok(AuthLookup::answers(
            LookupRecords::new(LookupOptions::default(), Arc::new(records)),
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
        _name: &hickory_proto::rr::LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

async fn ask_multi(handler: &DoggyDnsHandler, name: &str) -> Message {
    let capture = CapturingResponseHandler::new();
    handler
        .handle_request::<_, TokioTime>(&query(name), capture.clone())
        .await;
    capture
        .message
        .lock()
        .unwrap()
        .clone()
        .expect("response not captured")
}

fn first_octets(message: &Message) -> Vec<u8> {
    assert_eq!(message.metadata.response_code, ResponseCode::NoError);
    message
        .answers
        .iter()
        .map(|record| match &record.data {
            RData::A(address) => address.0.octets()[3],
            other => panic!("expected an A record, got {other:?}"),
        })
        .collect()
}
