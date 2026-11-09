//! Cache-writing middleware for the XDP fast path (Linux only).
//!
//! This submodule is gated by `middleware.rs`; it may use `aya`-backed types
//! without per-item `cfg` attributes.

use crate::cache::{DnsCacheMap, now_ktime_ns};
use crate::middleware::compute_cache_entry;
use async_trait::async_trait;
use doggy_dns_basic::XdpConfig;
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
use hickory_proto::op::ResponseCode;
use hickory_server::server::Request;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Middleware that populates the XDP DNS cache after each request.
///
/// After the authority chain resolves a query, this middleware writes the
/// response bytes and TTL into the BPF map so that subsequent identical
/// queries can be served directly by the XDP program in kernel space.
pub struct XdpCacheMiddleware {
    /// The handle handed over by the manager. It is never written through: it
    /// exists so each worker thread can duplicate it once (a duplicated file
    /// descriptor, i.e. the same kernel map) and then write without a lock.
    template: Arc<DnsCacheMap>,
    negative_ttl: u64,
    /// Set by the first failed duplication so the warning is emitted once per
    /// process instead of once per query: the failure repeats on every write
    /// that worker attempts, and the diagnostic value is in the first.
    warned_clone_failure: AtomicBool,
}

/// Sharing the template across threads is the whole point of the field, so
/// this assertion is the precondition made executable: `DnsCacheMap` is a
/// descriptor plus the map's descriptor data, nothing mutable inside.
const _: fn() = || {
    fn assert_sync<T: Sync>() {}
    assert_sync::<DnsCacheMap>();
};

thread_local! {
    /// This thread's own duplicate of the cache map, taken on its first write.
    ///
    /// A `BPF_MAP_UPDATE_ELEM` syscall is thread-safe against the same map, so
    /// handing every worker its own descriptor removes the lock rather than
    /// hiding it. Workers are long-lived, so the one duplication per thread
    /// amortises to nothing. The server installs exactly one of these
    /// middlewares per process, so one thread-local covers every write; a
    /// second instance sharing the process would have to key the handle by
    /// template as well.
    static CACHE_HANDLE: RefCell<Option<DnsCacheMap>> = const { RefCell::new(None) };
}

impl XdpCacheMiddleware {
    pub fn new(cache: DnsCacheMap, config: &XdpConfig) -> Self {
        Self {
            template: Arc::new(cache),
            negative_ttl: config.negative_ttl,
            warned_clone_failure: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Middleware for XdpCacheMiddleware {
    async fn before_request(&self, _request: &Request) -> MiddlewareAction {
        MiddlewareAction::Continue
    }

    async fn after_request(&self, request: &Request, _duration_ms: u64, outcome: &RequestOutcome) {
        // Only cache successful and NXDomain responses
        if outcome.response_code != ResponseCode::NoError
            && outcome.response_code != ResponseCode::NXDomain
        {
            return;
        }

        let Some(ref resp_bytes) = outcome.response_bytes else {
            return;
        };

        let request_info = match request.request_info() {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "failed to get request info for XDP cache");
                return;
            }
        };

        let qname = request_info.query.name();
        let qtype = u16::from(request_info.query.query_type());
        let qclass = u16::from(request_info.query.query_class());

        // Use kernel-compatible monotonic clock for TTL expiry
        let now_ns = now_ktime_ns();

        let (key, value) = compute_cache_entry(
            qname,
            qtype,
            qclass,
            outcome.response_code,
            resp_bytes,
            outcome.min_answer_ttl,
            self.negative_ttl,
            now_ns,
        );

        // The thread-local is borrowed and released inside this one synchronous
        // section: there is no `.await` between the borrow and its end, so the
        // handle cannot travel to another worker thread or outlive its own.
        CACHE_HANDLE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                match self.template.try_clone() {
                    Ok(handle) => *slot = Some(handle),
                    Err(error) => {
                        // Without a handle of its own this worker cannot write
                        // at all; the fast path just misses until one succeeds.
                        // The flag is diagnostic only, so `Relaxed` is enough;
                        // only the first failure is worth a line in the log.
                        if !self.warned_clone_failure.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                error = %error,
                                "failed to duplicate the XDP cache map for this worker, \
                                 entry not written"
                            );
                        }
                        return;
                    }
                }
            }
            let cache = slot.as_mut().expect("handle initialised above");
            if let Err(e) = cache.insert(&key, &value) {
                tracing::warn!(error = %e, qname = %qname, "failed to write XDP cache entry");
            }
        });
    }
}
