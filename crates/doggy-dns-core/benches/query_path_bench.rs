#![allow(clippy::disallowed_methods)]

//! The reloadable query path, measured on its own.
//!
//! Two gaps the existing `handler_bench` leaves open, both of which a reload
//! change can only be judged against:
//!
//! 1. Every handler bench builds `DoggyDnsHandler::new(vec![], …)`, so the
//!    middleware chain is empty. The shipped default is not: `build_server`
//!    always installs the logging middleware and the metrics middleware, and
//!    `middleware.logging` is the one reloadable knob that cannot join the
//!    query-path swap — the middleware chain owns the live XDP cache map and
//!    the OTel instruments and is never rebuilt. Here the logging middleware is
//!    installed on its own (the metrics one is `doggy-dns-observability`'s to
//!    price) and measured with its flag off, the cheapest state a reload can
//!    leave it in, and on.
//! 2. `handler_pipeline` says whether the whole path moved; it does not say how
//!    much of that is the per-query `ArcSwap` read. `query_path_read` prices
//!    that read against the shapes it replaced.
//!
//! The read is single-threaded by design. A resolution spans an `.await` and
//! hickory's `Server` owns the handler by value, so the query cannot borrow the
//! snapshot from a `load()` guard (`!Send`) and has to take ownership — the
//! refcount traffic below. What that ownership costs when many workers take it
//! at once shows up in `handler_pipeline_*` at `threads/1..=4`, which is where
//! the reload regression was visible; a synthetic thread knob here would price
//! tokio's task machinery rather than the read.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_core::middleware::logging::{LoggingMiddleware, LoggingToggle};
use doggy_dns_plugin::Middleware;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_server::net::runtime::TokioTime;
use hickory_server::server::RequestHandler;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

mod common;
use common::{NoOpResponseHandler, make_request};

/// The `DoggyDnsHandler` field is `Arc<ArcSwap<QueryPath>>` and `QueryPath` is
/// private, so this is the mirror of what one query reads out of it: the chain
/// that answers, and the flag that says whether its answers rotate.
struct Snapshot {
    chain: Arc<AuthorityChain>,
    rotate: bool,
}

impl Snapshot {
    fn new(chain: Arc<AuthorityChain>) -> Self {
        Self {
            chain,
            rotate: false,
        }
    }
}

/// The work one read has to do with whatever it got: look at both fields, so no
/// variant is cheaper because it touched less.
fn read(snapshot: &Snapshot) -> usize {
    snapshot.chain.len() + usize::from(black_box(snapshot.rotate))
}

/// What one query costs, per access shape. The control block, and therefore the
/// contended cache line, is the same `Arc<Snapshot>` in all four.
///
/// - `plain`: dereference an owned `Arc` field. The floor — what a handler
///   without hot reload pays, and what `swap` has to come back to.
/// - `clone`: increment and decrement that same `Arc`. The ownership cost a
///   query needs to survive the resolution, with no `ArcSwap` involved.
/// - `swap`: `clone` plus the RCU publish check, which is what
///   `ArcSwap::load_full` is: `Arc::clone(&self.load())`.
/// - `cached`: a generation counter and a private `Arc`, reused while the number
///   says nothing changed. The alternative that would remove the refcount pair,
///   priced here against the read it would replace.
fn bench_query_path_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_path_read");
    let chain = Arc::new(AuthorityChain::new(vec![]));
    let snapshot = Arc::new(Snapshot::new(Arc::clone(&chain)));
    let path = Arc::new(arc_swap::ArcSwap::<Snapshot>::from_pointee(Snapshot::new(
        Arc::clone(&chain),
    )));
    let generation = Arc::new(AtomicU64::new(0));

    group.bench_function("plain", |b| {
        let snapshot = Arc::clone(&snapshot);
        b.iter(|| {
            // Opaque to the compiler, or the whole read is loop-invariant and
            // vanishes out of the measured loop.
            let snapshot = black_box(&snapshot);
            read(snapshot)
        });
    });

    group.bench_function("clone", |b| {
        let snapshot = black_box(Arc::clone(&snapshot));
        b.iter(|| {
            let owned = Arc::clone(&snapshot);
            read(&owned)
        });
    });

    group.bench_function("swap", |b| {
        let path = Arc::clone(&path);
        b.iter(|| {
            let owned = path.load_full();
            read(&owned)
        });
    });

    // A reload is a per-edit event and a read is a per-query event, so the
    // counter is never stored to here: the half worth pricing is the hit path.
    group.bench_function("cached", |b| {
        let path = Arc::clone(&path);
        let generation = Arc::clone(&generation);
        let mut held: Option<(u64, Arc<Snapshot>)> = None;
        b.iter(move || {
            let seen = generation.load(Ordering::Relaxed);
            match &held {
                Some((age, snapshot)) if *age == seen => read(snapshot),
                _ => {
                    let snapshot = path.load_full();
                    let answers = read(&snapshot);
                    held = Some((seen, snapshot));
                    answers
                }
            }
        });
    });

    group.finish();
}

/// The NXDOMAIN pipeline with the middleware chain the default config installs.
///
/// `off` is the reloadable `middleware.logging` flag switched off: the two
/// trait calls and the atomic load, no log line. `on` additionally reaches
/// `tracing::info!`, and with no subscriber installed in this process that is a
/// floor for logging-on, not its shipped cost — the recorder, the formatter and
/// the writer are `doggy-dns-observability`'s to measure.
fn bench_handler_pipeline_logging_middleware(c: &mut Criterion) {
    let mut group = c.benchmark_group("handler_pipeline_logging_middleware");
    for label in ["off", "on"] {
        for threads in 1..=4usize {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(threads)
                .enable_all()
                .build()
                .expect("tokio runtime");
            let middlewares: Vec<Arc<dyn Middleware>> = vec![Arc::new(
                LoggingMiddleware::with_toggle(LoggingToggle::new(label == "on")),
            )];
            let handler = DoggyDnsHandler::new(
                middlewares,
                Arc::new(AuthorityChain::new(vec![])),
                false,
                AnswerRotation::new(false),
            );
            let request = make_request("unknown.example.com.");
            group.bench_with_input(
                BenchmarkId::new(format!("{label}/threads"), threads),
                &threads,
                |b, _| {
                    b.to_async(&rt).iter(|| async {
                        black_box(
                            handler
                                .handle_request::<_, TokioTime>(&request, NoOpResponseHandler)
                                .await,
                        )
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_query_path_read,
    bench_handler_pipeline_logging_middleware,
);
criterion_main!(benches);
