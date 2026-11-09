#![allow(clippy::disallowed_methods)]

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::mapping::{service_name_from_name, to_dns_records};
use doggy_dns_nacos::test_support::qname;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceFilter};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupOptions, ZoneHandler};
use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::sync::Arc;

fn make_populated_cache(count: usize) -> Arc<DashMap<String, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    for i in 0..count {
        let key = format!("service-{}", i);
        let instances = vec![CachedInstance {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, (i / 256) as u8, (i % 256) as u8)),
            port: 8080,
            healthy: true,
        }];
        cache.insert(key, instances);
    }
    cache
}

/// The authority lookup against the 100-service cache, for one query name.
fn bench_authority_lookup(c: &mut Criterion, group_name: &str, query: &str) {
    let cache = make_populated_cache(100);
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);
    let name = qname(query);

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
                    authority
                        .lookup(&name, RecordType::A, None, LookupOptions::default())
                        .await,
                )
            });
        });
    }
    group.finish();
}

fn bench_nacos_authority_lookup_hit(c: &mut Criterion) {
    bench_authority_lookup(
        c,
        "nacos_authority_lookup_hit",
        "service-50.default_group.public.nacos.local.",
    );
}

fn bench_nacos_authority_lookup_miss(c: &mut Criterion) {
    bench_authority_lookup(
        c,
        "nacos_authority_lookup_miss",
        "unknown-svc.default_group.public.nacos.local.",
    );
}

fn bench_service_name_from_name_hit(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    let name = LowerName::from(Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap());
    c.bench_function("service_name_from_name_hit", |b| {
        b.iter(|| {
            black_box(service_name_from_name(
                &name,
                &zone,
                "DEFAULT_GROUP",
                "public",
            ))
        });
    });
}

fn bench_service_name_from_name_miss(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    // Two labels above the zone: too few to carry group and namespace.
    let name = LowerName::from(Name::from_str("my-svc.group.nacos.local.").unwrap());
    c.bench_function("service_name_from_name_miss", |b| {
        b.iter(|| {
            black_box(service_name_from_name(
                &name,
                &zone,
                "DEFAULT_GROUP",
                "public",
            ))
        });
    });
}

fn bench_service_name_dotted_service(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    let name = LowerName::from(
        Name::from_str("com.example.api.DEFAULT_GROUP.public.nacos.local.").unwrap(),
    );
    c.bench_function("service_name_dotted_service", |b| {
        b.iter(|| {
            black_box(service_name_from_name(
                &name,
                &zone,
                "DEFAULT_GROUP",
                "public",
            ))
        });
    });
}

/// Names shaped like the traffic the filter sees: bare application names that
/// the built-in rule selects, plus one dotted and one `group@@service` shape it
/// rejects.
const FILTER_NAMES: [&str; 6] = [
    "order-service",
    "ORDER_svc-1",
    "a1",
    "inventory",
    "com.foo.Bar",
    "DEFAULT_GROUP@@order",
];

fn bench_service_filter(c: &mut Criterion, group_name: &str, filter: ServiceFilter) {
    // One iteration does 600 decisions, not 6: at a few nanoseconds per call
    // criterion's per-iteration overhead would otherwise be most of the number.
    const ROUNDS: usize = 100;
    c.bench_function(group_name, |b| {
        b.iter(|| {
            let mut selected = 0;
            for _ in 0..ROUNDS {
                for name in FILTER_NAMES {
                    selected += filter.is_selected(black_box(name)) as usize;
                }
            }
            black_box(selected)
        })
    });
}

/// The built-in rule without `service_pattern` configured.
fn bench_service_filter_default(c: &mut Criterion) {
    bench_service_filter(
        c,
        "service_filter_default",
        ServiceFilter::from_config(None).expect("the built-in rule"),
    );
}

/// The same rule spelled as a configured regex, which is the shape the default
/// rule took before the byte-wise shortcut.
fn bench_service_filter_regex(c: &mut Criterion) {
    bench_service_filter(
        c,
        "service_filter_regex",
        ServiceFilter::from_config(Some("[A-Za-z0-9_-]+")).expect("the explicit default pattern"),
    );
}

fn bench_to_dns_records_10_ips(c: &mut Criterion) {
    let name = LowerName::from(Name::from_str("my-svc.nacos.local.").unwrap());
    let instances: Vec<CachedInstance> = (0..10)
        .map(|i| CachedInstance {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)),
            port: 8080,
            healthy: true,
        })
        .collect();
    c.bench_function("to_dns_records_10_ips", |b| {
        b.iter(|| black_box(to_dns_records(&name, &instances, 60, RecordType::A)));
    });
}

criterion_group!(
    benches,
    bench_nacos_authority_lookup_hit,
    bench_nacos_authority_lookup_miss,
    bench_service_name_from_name_hit,
    bench_service_name_from_name_miss,
    bench_service_name_dotted_service,
    bench_service_filter_default,
    bench_service_filter_regex,
    bench_to_dns_records_10_ips,
);
criterion_main!(benches);
