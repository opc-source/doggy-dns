#![allow(clippy::disallowed_methods)]

//! A config reload reaches the handler that is already answering traffic.
//!
//! hickory's `Server` owns the handler by value, so the only thing a reload can
//! change is what the handler reads through: the authority chain (swapped
//! wholesale) and rotation (a flag on the shared cursor). These tests build a
//! handler, take its `ReloadHandle`, then keep querying the same handler to
//! prove the swap is what the next answer sees.

use async_trait::async_trait;
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::NetError;
use hickory_server::net::runtime::TokioTime;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupOptions, MessageResponse, ZoneHandler,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

const ZONE: &str = "nacos.local";
const QNAME: &str = "my-svc.DEFAULT_GROUP.public.nacos.local.";

/// Cache the given addresses under one service key, as `NacosAuthority` reads it.
fn authority_with(ips: &[&str], ttl: u32) -> NacosAuthority {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey::new("my-svc", "default_group", "public"),
        ips.iter()
            .map(|ip| CachedInstance {
                ip: ip.parse().unwrap(),
                port: 8080,
                healthy: true,
            })
            .collect::<Vec<_>>(),
    );
    NacosAuthority::new(cache, ZONE, ttl)
}

fn chain_with(ips: &[&str], ttl: u32) -> AuthorityChain {
    AuthorityChain::new(vec![(
        format!("nacos[{ZONE}]"),
        Arc::new(authority_with(ips, ttl)) as Arc<dyn ZoneHandler>,
    )])
}

fn request() -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    let mut query = Query::new();
    query.set_name(Name::from_str(QNAME).unwrap());
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

#[derive(Clone)]
struct CapturingHandler {
    message: Arc<std::sync::Mutex<Option<Message>>>,
}

impl CapturingHandler {
    fn new() -> Self {
        Self {
            message: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn message(&self) -> Message {
        self.message
            .lock()
            .unwrap()
            .clone()
            .expect("no response captured")
    }

    /// Answer IPs in wire order.
    fn answer_ips(&self) -> Vec<String> {
        self.message()
            .answers
            .iter()
            .filter_map(|record| match &record.data {
                RData::A(address) => Some(address.0.to_string()),
                _ => None,
            })
            .collect()
    }

    fn first_answer_ttl(&self) -> u32 {
        self.message().answers.first().expect("no answers").ttl
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
        *self.message.lock().unwrap() = Some(message);
        Ok(info)
    }
}

async fn query(handler: &DoggyDnsHandler) -> CapturingHandler {
    let capture = CapturingHandler::new();
    handler
        .handle_request::<_, TokioTime>(&request(), capture.clone())
        .await;
    capture
}

/// A chain that skips everything, standing in for a plugin removed by a reload.
struct SkipZoneHandler;

#[async_trait]
impl ZoneHandler for SkipZoneHandler {
    fn zone_type(&self) -> hickory_server::zone_handler::ZoneType {
        hickory_server::zone_handler::ZoneType::External
    }

    fn axfr_policy(&self) -> hickory_server::zone_handler::AxfrPolicy {
        hickory_server::zone_handler::AxfrPolicy::Deny
    }

    fn origin(&self) -> &hickory_proto::rr::LowerName {
        static ORIGIN: std::sync::LazyLock<hickory_proto::rr::LowerName> =
            std::sync::LazyLock::new(|| {
                hickory_proto::rr::LowerName::from(Name::from_str("empty.local.").unwrap())
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
        LookupControlFlow::Skip
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

#[tokio::test]
async fn swapped_chain_answers_the_next_query() {
    let handler = DoggyDnsHandler::new(
        vec![],
        Arc::new(chain_with(&["10.0.0.1"], 6)),
        false,
        AnswerRotation::new(false),
    );
    let reload = handler.reload_handle();

    assert_eq!(
        query(&handler).await.answer_ips(),
        vec!["10.0.0.1".to_string()]
    );

    reload.swap_authority_chain(chain_with(&["10.0.0.2", "10.0.0.3"], 11));

    let capture = query(&handler).await;
    assert_eq!(
        capture.answer_ips(),
        vec!["10.0.0.2".to_string(), "10.0.0.3".to_string()],
        "the reloaded chain, not the start-up one, must answer"
    );
    assert_eq!(
        capture.first_answer_ttl(),
        11,
        "a reloaded ttl must reach the records the handler emits"
    );
}

#[tokio::test]
async fn swapped_chain_can_drop_every_authority() {
    let handler = DoggyDnsHandler::new(
        vec![],
        Arc::new(chain_with(&["10.0.0.1"], 6)),
        false,
        AnswerRotation::new(false),
    );
    let reload = handler.reload_handle();
    assert_eq!(
        query(&handler).await.answer_ips(),
        vec!["10.0.0.1".to_string()]
    );

    // All plugins disabled or removed: the chain is empty and every query
    // falls through to NXDOMAIN rather than keeping the old answers.
    reload.swap_authority_chain(AuthorityChain::new(vec![(
        "skip".to_string(),
        Arc::new(SkipZoneHandler) as Arc<dyn ZoneHandler>,
    )]));

    let capture = query(&handler).await;
    assert_eq!(
        capture.message().metadata.response_code,
        ResponseCode::NXDomain,
        "an empty reloaded chain must not keep answering"
    );
}

#[tokio::test]
async fn rotation_toggles_on_the_live_handler() {
    let handler = DoggyDnsHandler::new(
        vec![],
        Arc::new(chain_with(&["10.0.0.1", "10.0.0.2", "10.0.0.3"], 6)),
        false,
        AnswerRotation::new(false),
    );
    let reload = handler.reload_handle();

    let mut stable = Vec::new();
    for _ in 0..3 {
        stable.push(query(&handler).await.answer_ips()[0].clone());
    }
    assert_eq!(
        stable,
        vec!["10.0.0.1".to_string(); 3],
        "rotation off keeps the authority's order"
    );

    reload.set_rotation(true);
    let mut walked = Vec::new();
    for _ in 0..3 {
        walked.push(query(&handler).await.answer_ips()[0].clone());
    }
    assert_eq!(
        walked,
        vec![
            "10.0.0.1".to_string(),
            "10.0.0.2".to_string(),
            "10.0.0.3".to_string()
        ],
        "turning rotation on mid-life must start walking the instance list"
    );

    reload.set_rotation(false);
    assert_eq!(
        query(&handler).await.answer_ips()[0],
        "10.0.0.1",
        "turning rotation off returns to the authority's own order"
    );
}

#[tokio::test]
async fn swapping_under_concurrent_queries_answers_valid_generations() {
    // Every answer must come wholly from one generation: a query reads the
    // chain once, so it can never mix the old and the new authority.
    let handler = Arc::new(DoggyDnsHandler::new(
        vec![],
        Arc::new(chain_with(&["10.0.0.1"], 6)),
        false,
        AnswerRotation::new(false),
    ));
    let reload = handler.reload_handle();

    let readers: Vec<_> =
        (0..4)
            .map(|_| {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    for _ in 0..50 {
                        let capture = CapturingHandler::new();
                        handler
                            .handle_request::<_, TokioTime>(&request(), capture.clone())
                            .await;
                        let message = capture.message();
                        assert_eq!(message.metadata.response_code, ResponseCode::NoError);
                        seen.extend(message.answers.iter().filter_map(
                            |record| match &record.data {
                                RData::A(address) => Some(address.0.to_string()),
                                _ => None,
                            },
                        ));
                    }
                    seen
                })
            })
            .collect();

    let swapper = {
        let reload = reload.clone();
        tokio::spawn(async move {
            for generation in 0..20u8 {
                let ip = format!("10.1.0.{generation}");
                reload.swap_authority_chain(chain_with(&[ip.as_str()], 6));
                tokio::task::yield_now().await;
            }
        })
    };

    for reader in readers {
        let seen = reader.await.expect("reader task panicked");
        assert!(
            !seen.is_empty(),
            "concurrent queries must keep being answered"
        );
        for ip in seen {
            assert!(
                ip == "10.0.0.1" || ip.starts_with("10.1.0."),
                "answer {ip} belongs to neither chain generation"
            );
        }
    }
    swapper.await.expect("swapper task panicked");
}
