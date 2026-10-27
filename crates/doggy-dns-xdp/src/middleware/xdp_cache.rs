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
use std::sync::Arc;
use tokio::sync::Mutex;

/// Middleware that populates the XDP DNS cache after each request.
///
/// After the authority chain resolves a query, this middleware writes the
/// response bytes and TTL into the BPF map so that subsequent identical
/// queries can be served directly by the XDP program in kernel space.
pub struct XdpCacheMiddleware {
    cache: Arc<Mutex<DnsCacheMap>>,
    negative_ttl: u64,
}

impl XdpCacheMiddleware {
    pub fn new(cache: DnsCacheMap, config: &XdpConfig) -> Self {
        Self {
            cache: Arc::new(Mutex::new(cache)),
            negative_ttl: config.negative_ttl,
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
            resp_bytes.clone(),
            outcome.min_answer_ttl,
            self.negative_ttl,
            now_ns,
        );

        let mut cache = self.cache.lock().await;
        if let Err(e) = cache.insert(&key, &value) {
            tracing::warn!(error = %e, qname = %qname, "failed to write XDP cache entry");
        }
    }
}
