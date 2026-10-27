use crate::cache::make_cache_key;
use crate::types::{CacheKey, CacheValue};
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::Name;

/// Compute cache key and value for a DNS response.
///
/// This is a pure function (no side effects) exported for testing.
///
/// For positive responses (`NoError`), the TTL is taken from `min_answer_ttl`
/// (falling back to `negative_ttl`). For negative responses (`NXDomain`),
/// `negative_ttl` is used. `ServFail` and other error codes still produce a
/// structurally valid entry, but the caller (middleware) should filter them
/// out before calling this function.
#[allow(clippy::too_many_arguments)]
pub fn compute_cache_entry(
    qname: &Name,
    qtype: u16,
    qclass: u16,
    response_code: ResponseCode,
    resp_bytes: Vec<u8>,
    min_answer_ttl: Option<u32>,
    negative_ttl: u64,
    now_ns: u64,
) -> (CacheKey, CacheValue) {
    let key = make_cache_key(qname, qtype, qclass);

    let is_negative = response_code == ResponseCode::NXDomain;
    let ttl = if is_negative {
        negative_ttl
    } else {
        min_answer_ttl.unwrap_or(negative_ttl as u32) as u64
    };

    let expires_at = now_ns + ttl * 1_000_000_000;

    let value = if is_negative {
        CacheValue::new_negative(resp_bytes, expires_at)
    } else {
        CacheValue::new_positive(resp_bytes, expires_at)
    };

    (key, value)
}

// ---------------------------------------------------------------------------
// Linux-only: XdpCacheMiddleware (requires DnsCacheMap which uses aya)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
use crate::cache::{DnsCacheMap, now_ktime_ns};
#[cfg(target_os = "linux")]
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use doggy_dns_basic::XdpConfig;
#[cfg(target_os = "linux")]
use doggy_dns_plugin::{Middleware, MiddlewareAction, RequestOutcome};
#[cfg(target_os = "linux")]
use hickory_server::server::Request;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;

/// Middleware that populates the XDP DNS cache after each request.
///
/// After the authority chain resolves a query, this middleware writes the
/// response bytes and TTL into the BPF map so that subsequent identical
/// queries can be served directly by the XDP program in kernel space.
#[cfg(target_os = "linux")]
pub struct XdpCacheMiddleware {
    cache: Arc<Mutex<DnsCacheMap>>,
    negative_ttl: u64,
}

#[cfg(target_os = "linux")]
impl XdpCacheMiddleware {
    pub fn new(cache: DnsCacheMap, config: &XdpConfig) -> Self {
        Self {
            cache: Arc::new(Mutex::new(cache)),
            negative_ttl: config.negative_ttl,
        }
    }
}

#[cfg(target_os = "linux")]
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
