#![allow(clippy::disallowed_methods)]

// The handler benchmark queries the same wire shape the tests build, so it
// shares their builder; `#[path]` is how a bench target reaches a test module.
#[path = "../tests/common/mod.rs"]
mod common;

use async_trait::async_trait;
use common::dns_request;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::{CatalogAnswer, NacosAuthority, ServiceCatalog};
use doggy_dns_nacos::watcher::CachedInstance;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Edns, Header, HeaderCounts, Query};
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType, rdata::A};
use hickory_server::net::NetError;
use hickory_server::net::runtime::TokioTime;
use hickory_server::resolver::lookup::Lookup;
use hickory_server::server::{Request, RequestHandler, RequestInfo, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, MessageResponse, ZoneHandler,
    ZoneType,
};
use std::hint::black_box;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Clone)]
struct NoOpResponseHandler;

#[async_trait]
impl ResponseHandler for NoOpResponseHandler {
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
        Ok(ResponseInfo::from(Header {
            metadata,
            counts: HeaderCounts::default(),
        }))
    }
}

fn make_request(domain: &str) -> Request {
    dns_request(1234, domain, RecordType::A, None)
}

fn make_request_edns(domain: &str, max_payload: u16) -> Request {
    let mut edns = Edns::new();
    edns.set_max_payload(max_payload);
    dns_request(1234, domain, RecordType::A, Some(edns))
}

/// One pipeline case against the thread-count sweep every group shares.
fn bench_pipeline(c: &mut Criterion, group_name: &str, handler: DoggyDnsHandler, request: Request) {
    let mut group = c.benchmark_group(group_name);
    for threads in 1..=4 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .unwrap();
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, _| {
            b.to_async(&rt).iter(|| async {
                black_box(
                    handler
                        .handle_request::<_, TokioTime>(&request, NoOpResponseHandler)
                        .await,
                )
            });
        });
    }
    group.finish();
}

/// Nacos authority over `instance_count` instances of `my-svc`, answering from
/// the move path (`LookupRecords::Section`).
fn nacos_handler(instance_count: u8, xdp_enabled: bool, rotate: bool) -> DoggyDnsHandler {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "my-svc".to_string(),
        (1..=instance_count)
            .map(|i| CachedInstance {
                ip: Ipv4Addr::new(10, 0, 0, i).into(),
                port: 8080,
                healthy: true,
            })
            .collect(),
    );
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 60);
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    DoggyDnsHandler::new(vec![], chain, xdp_enabled, AnswerRotation::new(rotate))
}

const SVC_QUERY: &str = "my-svc.DEFAULT_GROUP.public.nacos.local.";

fn bench_handler_pipeline_nacos_hit(c: &mut Criterion) {
    bench_pipeline(
        c,
        "handler_pipeline_nacos_hit",
        nacos_handler(1, false, false),
        make_request(SVC_QUERY),
    );
}

/// Full pipeline on an in-zone name the cache does not hold.
///
/// The existing `handler_pipeline_nxdomain` uses an empty chain, which never
/// reaches the authoritative negative branch: the chain only fetches the apex
/// SOA when the responding handler owns its zone. This case is what the
/// apex-SOA caching work measures, and the substitute for a Nacos end-to-end
/// scenario (the server aborts boot when Nacos is unreachable, so CI cannot
/// stress that path without a live Nacos).
fn bench_handler_pipeline_nacos_nxdomain(c: &mut Criterion) {
    bench_pipeline(
        c,
        "handler_pipeline_nacos_nxdomain",
        nacos_handler(1, false, false),
        make_request("unknown-svc.DEFAULT_GROUP.public.nacos.local."),
    );
}

fn bench_handler_pipeline_nxdomain(c: &mut Criterion) {
    let chain = Arc::new(AuthorityChain::new(vec![]));
    let handler = DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false));
    bench_pipeline(
        c,
        "handler_pipeline_nxdomain",
        handler,
        make_request("unknown.example.com."),
    );
}

/// XDP cache payload on the answer path: `serialize_for_xdp` + `min_ttl` run
/// only when the handler is built with XDP enabled.
fn bench_handler_pipeline_nacos_hit_xdp(c: &mut Criterion) {
    bench_pipeline(
        c,
        "handler_pipeline_nacos_hit_xdp",
        nacos_handler(4, true, false),
        make_request(SVC_QUERY),
    );
}

/// The serialize-then-skip path: EDNS advertises 4096 so the emitter writes the
/// whole answer, and 40 A records (~734 bytes on the wire) then exceed the
/// 512-byte cache capacity. Without EDNS the emitter would truncate at 512 and
/// the TC bit would skip it first, measuring a different branch.
fn bench_handler_pipeline_nacos_hit_xdp_oversize(c: &mut Criterion) {
    bench_pipeline(
        c,
        "handler_pipeline_nacos_hit_xdp_oversize",
        nacos_handler(40, true, false),
        make_request_edns(SVC_QUERY, 4096),
    );
}

/// Rotation on a multi-record answer: the atomic cursor plus `rotate_left`
/// that single-record groups never reach.
fn bench_handler_pipeline_nacos_hit_rotation(c: &mut Criterion) {
    bench_pipeline(
        c,
        "handler_pipeline_nacos_hit_rotation",
        nacos_handler(4, false, true),
        make_request(SVC_QUERY),
    );
}

/// The XDP negative payload: `xdp_error_payload` serializes the SOA-bearing
/// NXDOMAIN that the cache serves during an outage.
fn bench_handler_pipeline_nacos_nxdomain_xdp(c: &mut Criterion) {
    bench_pipeline(
        c,
        "handler_pipeline_nacos_nxdomain_xdp",
        nacos_handler(1, true, false),
        make_request("unknown-svc.DEFAULT_GROUP.public.nacos.local."),
    );
}

/// A catalog whose backend is down: the authority answers SERVFAIL and the
/// chain attaches the cached apex SOA, the outage path a live-Nacos bench
/// cannot reach.
struct UnavailableCatalog;

impl ServiceCatalog for UnavailableCatalog {
    async fn instances(&self, _service: &str) -> CatalogAnswer {
        CatalogAnswer::Unavailable
    }
}

fn bench_handler_pipeline_nacos_servfail(c: &mut Criterion) {
    let authority = NacosAuthority::new(
        Arc::new(UnavailableCatalog),
        "nacos.local",
        "DEFAULT_GROUP",
        "public",
        60,
    );
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    let handler = DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false));
    bench_pipeline(
        c,
        "handler_pipeline_nacos_servfail",
        handler,
        make_request(SVC_QUERY),
    );
}

/// The clone fallback in `take_answer_records`: `native`/`forward` answer with
/// `AuthLookup::Resolved`, whose records live behind the resolver's own
/// storage. A warm resolver cache hands out a cloned `Lookup` per query, which
/// is what this fake reproduces without a network.
struct ResolvedZoneHandler {
    origin: LowerName,
    lookup: Lookup,
}

impl ResolvedZoneHandler {
    fn new(domain: &str, instance_count: u8) -> Self {
        let name = Name::from_str(domain).unwrap();
        let mut query = Query::new();
        query.set_name(name.clone());
        query.set_query_type(RecordType::A);
        let records: Vec<Record> = (1..=instance_count)
            .map(|i| Record::from_rdata(name.clone(), 60, RData::A(A(Ipv4Addr::new(10, 0, 0, i)))))
            .collect();
        Self {
            origin: LowerName::from(Name::root()),
            lookup: Lookup::new_with_max_ttl(query, records),
        }
    }
}

#[async_trait]
impl ZoneHandler for ResolvedZoneHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::External
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        &self.origin
    }

    async fn lookup(
        &self,
        _name: &LowerName,
        _rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Continue(Ok(AuthLookup::Resolved(self.lookup.clone())))
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

fn bench_handler_pipeline_resolved_clone(c: &mut Criterion) {
    let chain = Arc::new(AuthorityChain::new(vec![(
        "resolved".to_string(),
        Arc::new(ResolvedZoneHandler::new("resolved.example.com.", 4)),
    )]));
    let handler = DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false));
    bench_pipeline(
        c,
        "handler_pipeline_resolved_clone",
        handler,
        make_request("resolved.example.com."),
    );
}

criterion_group!(
    benches,
    bench_handler_pipeline_nacos_hit,
    bench_handler_pipeline_nacos_nxdomain,
    bench_handler_pipeline_nxdomain,
    bench_handler_pipeline_nacos_hit_xdp,
    bench_handler_pipeline_nacos_hit_xdp_oversize,
    bench_handler_pipeline_nacos_hit_rotation,
    bench_handler_pipeline_nacos_nxdomain_xdp,
    bench_handler_pipeline_nacos_servfail,
    bench_handler_pipeline_resolved_clone,
);
criterion_main!(benches);
