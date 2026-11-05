#![allow(clippy::disallowed_methods)]

//! Allocation counts on the Nacos resolve path.
//!
//! Timings on a shared runner are easy to wave away; an allocation count is not.
//! This wraps the system allocator and measures one `lookup` call, which is the
//! unit of work the hot-path refactor owns. The budgets were measured with a zero
//! budget on this file's first run, not guessed: one answering instance costs 5
//! allocations and the in-zone miss costs 1. The `Vec<Record>` and the
//! per-record answer `Name` are the sites a regression
//! would grow; the count is the contract, not a sum of its parts.
//!
//! Both measurements live in one test on purpose: libtest runs tests in a file
//! concurrently, and a second thread allocating inside the measured window would
//! make the count meaningless.

use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::CachedInstance;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::alloc::{GlobalAlloc, Layout, System};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: this forwards every request to `System` unchanged and only counts the
// allocations it passes through. `dealloc` deliberately does not count, so the
// figure is "allocations requested", which is what the budgets describe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

type MapCatalog = DashMap<String, Vec<CachedInstance>>;

fn authority() -> NacosAuthority<MapCatalog> {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "user-service".to_string(),
        vec![CachedInstance {
            ip: IpAddr::from([10, 0, 1, 5]),
            port: 8080,
            healthy: true,
        }],
    );
    NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6)
}

/// Count allocations made by one `lookup` call.
///
/// The runtime is passed in rather than built here: building it allocates, and
/// the counter is reset at the top of this function, so a runtime built inside
/// the window would report its own allocations as the cost of a lookup.
fn measure(
    query: &str,
    runtime: &tokio::runtime::Runtime,
    authority: &NacosAuthority<MapCatalog>,
    rtype: RecordType,
) -> usize {
    let lower = LowerName::from(Name::from_str(query).unwrap());
    ALLOCS.store(0, Ordering::Relaxed);
    let result = runtime.block_on(authority.lookup(&lower, rtype, None, LookupOptions::default()));
    let count = ALLOCS.load(Ordering::Relaxed);
    assert!(
        matches!(result, LookupControlFlow::Continue(_)),
        "{query}: expected a Continue result inside the zone"
    );
    count
}

#[test]
fn hit_and_miss_paths_allocate_within_budget() {
    let authority = authority();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // `user-service` is cached; `unknown-svc` is in the zone but not in the
    // cache, which is the NXDOMAIN path.
    let hit = measure(
        "user-service.default_group.public.nacos.local.",
        &runtime,
        &authority,
        RecordType::A,
    );
    let miss = measure(
        "unknown-svc.default_group.public.nacos.local.",
        &runtime,
        &authority,
        RecordType::A,
    );

    // Measured: 5 allocations for one answering instance and 1 for the in-zone
    // miss. The budget equals the measurement - no slack - so the next
    // `to_lowercase`, `format!` or extra `Vec` clone on this path fails the suite
    // instead of the next review. The lookup borrows the service name out of the
    // `LowerName` it already holds, so a bare application name — the shape the
    // default `service_pattern` selects — costs no string at all; the composite
    // key this replaced built one per query. The authority's catalog is generic,
    // so reading it costs no boxed future; the answer name is built per record
    // from the `LowerName` the lookup already holds, so a NODATA answer allocates
    // no name at all. Changing the
    // seed above means re-measuring; that is deliberate, not routine.
    const HIT_BUDGET: usize = 5;
    const MISS_BUDGET: usize = 1;
    assert!(
        hit <= HIT_BUDGET,
        "hit path: {hit} allocations (budget {HIT_BUDGET}) - record the measured count"
    );
    assert!(
        miss <= MISS_BUDGET,
        "miss path: {miss} allocations (budget {MISS_BUDGET}) - record the measured count"
    );
}
