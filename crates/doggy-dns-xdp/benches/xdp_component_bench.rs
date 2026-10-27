use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use doggy_dns_xdp::cache::{fnv1a_64, make_cache_key};
use doggy_dns_xdp::types::CacheValue;
use hickory_proto::rr::Name;

fn bench_fnv1a_hash(c: &mut Criterion) {
    let names = [
        "svc-1.nacos.default.doggy.test",
        "api-gateway.production.cluster.local",
        "a.b.c.d.e.f.g.h.i.j.k.l.m.n.o.p",
    ];

    c.bench_function("fnv1a_64_short", |b| {
        b.iter(|| fnv1a_64(black_box(names[0].as_bytes())))
    });

    c.bench_function("fnv1a_64_long", |b| {
        b.iter(|| fnv1a_64(black_box(names[2].as_bytes())))
    });
}

fn bench_make_cache_key(c: &mut Criterion) {
    let name = Name::from_ascii("svc-1.nacos.default.doggy.test.").expect("valid benchmark name");

    c.bench_function("make_cache_key", |b| {
        b.iter(|| make_cache_key(black_box(&name), black_box(1), black_box(1)))
    });
}

fn bench_cache_value_construction(c: &mut Criterion) {
    let resp_data = vec![0u8; 128];

    c.bench_function("cache_value_positive", |b| {
        b.iter(|| CacheValue::new_positive(black_box(resp_data.clone()), black_box(1_000_000_000)))
    });

    c.bench_function("cache_value_negative", |b| {
        b.iter(|| CacheValue::new_negative(black_box(resp_data.clone()), black_box(1_000_000_000)))
    });
}

criterion_group!(
    benches,
    bench_fnv1a_hash,
    bench_make_cache_key,
    bench_cache_value_construction,
);
criterion_main!(benches);
