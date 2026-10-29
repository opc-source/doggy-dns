#![allow(clippy::disallowed_methods)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::mapping::{service_key_from_name, to_dns_records};
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupOptions, ZoneHandler};
use std::hint::black_box;
use std::str::FromStr;
use std::sync::Arc;

fn make_populated_cache(count: usize) -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    for i in 0..count {
        let key = ServiceKey::new(&format!("service-{}", i), "default_group", "public");
        let instances = vec![CachedInstance {
            ip: format!("10.0.{}.{}", i / 256, i % 256),
            port: 8080,
            healthy: true,
        }];
        cache.insert(key, instances);
    }
    cache
}

fn bench_nacos_authority_lookup_hit(c: &mut Criterion) {
    let cache = make_populated_cache(100);
    let authority = NacosAuthority::new(cache, "nacos.local", 6);
    let name =
        LowerName::from(Name::from_str("service-50.default_group.public.nacos.local.").unwrap());

    let mut group = c.benchmark_group("nacos_authority_lookup_hit");
    for threads in 1..=4 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .unwrap();
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, _| {
            b.to_async(&rt).iter(|| async {
                black_box(
                    authority
                        .lookup(&name, RecordType::A, None, LookupOptions::default())
                        .await,
                )
            });
        });
    }
    group.finish();
}

fn bench_nacos_authority_lookup_miss(c: &mut Criterion) {
    let cache = make_populated_cache(100);
    let authority = NacosAuthority::new(cache, "nacos.local", 6);
    let name =
        LowerName::from(Name::from_str("unknown-svc.default_group.public.nacos.local.").unwrap());

    let mut group = c.benchmark_group("nacos_authority_lookup_miss");
    for threads in 1..=4 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .unwrap();
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, _| {
            b.to_async(&rt).iter(|| async {
                black_box(
                    authority
                        .lookup(&name, RecordType::A, None, LookupOptions::default())
                        .await,
                )
            });
        });
    }
    group.finish();
}

fn bench_service_key_from_name_hit(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    let name = LowerName::from(Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap());
    c.bench_function("service_key_from_name_hit", |b| {
        b.iter(|| black_box(service_key_from_name(&name, &zone)));
    });
}

fn bench_service_key_from_name_miss(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    // Two labels above the zone: too few to carry group and namespace.
    let name = LowerName::from(Name::from_str("my-svc.group.nacos.local.").unwrap());
    c.bench_function("service_key_from_name_miss", |b| {
        b.iter(|| black_box(service_key_from_name(&name, &zone)));
    });
}

fn bench_service_key_dotted_service(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    let name = LowerName::from(
        Name::from_str("com.example.api.DEFAULT_GROUP.public.nacos.local.").unwrap(),
    );
    c.bench_function("service_key_dotted_service", |b| {
        b.iter(|| black_box(service_key_from_name(&name, &zone)));
    });
}

fn bench_to_dns_records_10_ips(c: &mut Criterion) {
    let name = Name::from_str("my-svc.nacos.local.").unwrap();
    let ips: Vec<String> = (0..10).map(|i| format!("10.0.0.{}", i)).collect();
    c.bench_function("to_dns_records_10_ips", |b| {
        b.iter(|| black_box(to_dns_records(&name, &ips, 60, RecordType::A)));
    });
}

criterion_group!(
    benches,
    bench_nacos_authority_lookup_hit,
    bench_nacos_authority_lookup_miss,
    bench_service_key_from_name_hit,
    bench_service_key_from_name_miss,
    bench_service_key_dotted_service,
    bench_to_dns_records_10_ips,
);
criterion_main!(benches);
