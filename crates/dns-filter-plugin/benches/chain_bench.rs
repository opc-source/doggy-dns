use criterion::{Criterion, criterion_group, criterion_main};
use dns_filter_plugin::authority_chain::AuthorityChain;
use hickory_proto::rr::{Name, RecordType};
use std::hint::black_box;
use std::str::FromStr;

fn bench_authority_chain_empty(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let chain = AuthorityChain::new(vec![]);
    let name = Name::from_str("example.com.").unwrap();

    c.bench_function("authority_chain_empty", |b| {
        b.to_async(&rt)
            .iter(|| async { black_box(chain.resolve(&name, RecordType::A, None).await) });
    });
}

criterion_group!(benches, bench_authority_chain_empty);
criterion_main!(benches);
