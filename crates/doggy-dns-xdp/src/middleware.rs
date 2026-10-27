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
//
// Gated as a submodule so the pure entry builder above builds everywhere and
// the middleware itself needs no item-level cfg attributes.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod xdp_cache;
#[cfg(target_os = "linux")]
pub use xdp_cache::XdpCacheMiddleware;
