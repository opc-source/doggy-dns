use async_trait::async_trait;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use dns_filter_core::handler::DnsFilterHandler;
use dns_filter_core::middleware::metrics::MetricsMiddleware;
use dns_filter_nacos::authority::NacosAuthority;
use dns_filter_nacos::watcher::{CachedInstance, ServiceKey};
use dns_filter_plugin::Middleware;
use dns_filter_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Message, MessageType, OpCode, Query};
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
        let header = *response.header();
        Ok(ResponseInfo::from(header))
    }
}

fn make_request(domain: &str) -> Request {
    let mut message = Message::new(1234, MessageType::Query, OpCode::Query);
    message.set_recursion_desired(true);
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
        ServiceKey {
            service_name: "my-svc".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "10.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    let metrics = Arc::new(MetricsMiddleware::new());
    let handler = DnsFilterHandler::new(vec![metrics as Arc<dyn Middleware>], chain);
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

fn bench_handler_pipeline_nxdomain(c: &mut Criterion) {
    let chain = Arc::new(AuthorityChain::new(vec![]));
    let handler = DnsFilterHandler::new(vec![], chain);
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
    bench_handler_pipeline_nxdomain,
);
criterion_main!(benches);
