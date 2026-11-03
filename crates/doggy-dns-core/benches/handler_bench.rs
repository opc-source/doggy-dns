#![allow(clippy::disallowed_methods)]

//! Handler pipeline cost, per answer shape.
//!
//! The `threads` parameter sizes the runtime a request is driven on. One request
//! runs at a time, so what a larger count changes is how much of tokio's worker
//! set contends for the cache lines the pipeline touches — which is why a
//! per-query shared refcount shows up here at all.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_server::net::runtime::TokioTime;
use hickory_server::server::RequestHandler;
use std::hint::black_box;
use std::sync::Arc;

mod common;
use common::{NoOpResponseHandler, make_request};

/// A name the Nacos authority holds an answer for.
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
