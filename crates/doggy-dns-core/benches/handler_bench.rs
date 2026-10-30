#![allow(clippy::disallowed_methods)]

use async_trait::async_trait;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Header, HeaderCounts, Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, Record, RecordType};
use hickory_server::net::NetError;
use hickory_server::net::runtime::TokioTime;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::MessageResponse;
use std::hint::black_box;
use std::net::{Ipv4Addr, SocketAddr};
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
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(RecordType::A);
    message.add_query(query);
    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

fn bench_handler_pipeline_nacos_hit(c: &mut Criterion) {
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
    let handler = DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false));
    let request = make_request("my-svc.DEFAULT_GROUP.public.nacos.local.");

    let mut group = c.benchmark_group("handler_pipeline_nacos_hit");
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

/// Full pipeline on an in-zone name the cache does not hold.
///
/// The existing `handler_pipeline_nxdomain` uses an empty chain, which never
/// reaches the authoritative negative branch: the chain only fetches the apex
/// SOA when the responding handler owns its zone. This case is what the
/// apex-SOA caching work measures, and the substitute for a Nacos end-to-end
/// scenario (the server aborts boot when Nacos is unreachable, so CI cannot
/// stress that path without a live Nacos).
fn bench_handler_pipeline_nacos_nxdomain(c: &mut Criterion) {
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
    let handler = DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false));
    let request = make_request("unknown-svc.DEFAULT_GROUP.public.nacos.local.");

    let mut group = c.benchmark_group("handler_pipeline_nacos_nxdomain");
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

fn bench_handler_pipeline_nxdomain(c: &mut Criterion) {
    let chain = Arc::new(AuthorityChain::new(vec![]));
    let handler = DoggyDnsHandler::new(vec![], chain, false, AnswerRotation::new(false));
    let request = make_request("unknown.example.com.");

    let mut group = c.benchmark_group("handler_pipeline_nxdomain");
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

criterion_group!(
    benches,
    bench_handler_pipeline_nacos_hit,
    bench_handler_pipeline_nacos_nxdomain,
    bench_handler_pipeline_nxdomain,
);
criterion_main!(benches);
