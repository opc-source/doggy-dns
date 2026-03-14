use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use dns_filter_nacos::authority::NacosAuthority;
use dns_filter_nacos::watcher::{CachedInstance, ServiceKey};
use dns_filter_plugin::authority_chain::AuthorityChain;
use hickory_proto::rr::{Name, RecordType};
use std::hint::black_box;
use std::str::FromStr;
use std::sync::Arc;

fn bench_authority_chain_empty(c: &mut Criterion) {
    let chain = AuthorityChain::new(vec![]);
    let name = Name::from_str("example.com.").unwrap();

    let mut group = c.benchmark_group("authority_chain_empty");
    for threads in 1..=4 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .unwrap();
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, _| {
            b.to_async(&rt)
                .iter(|| async { black_box(chain.resolve(&name, RecordType::A, None).await) });
        });
    }
    group.finish();
}

fn bench_authority_chain_nacos_hit(c: &mut Criterion) {
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
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);
    let name = Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap();

    let mut group = c.benchmark_group("authority_chain_nacos_hit");
    for threads in 1..=4 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .unwrap();
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, _| {
            b.to_async(&rt)
                .iter(|| async { black_box(chain.resolve(&name, RecordType::A, None).await) });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_authority_chain_empty,
    bench_authority_chain_nacos_hit,
);
criterion_main!(benches);
