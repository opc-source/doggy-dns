//! BPF map wrapper and kernel-monotonic clock helper (Linux only).
//!
//! This submodule is gated by `cache.rs`; everything here may use `aya` and
//! `libc` freely without per-item `cfg` attributes.

use anyhow::Result;
use aya::maps::{HashMap, Map, MapData};

use crate::types::{CacheKey, CacheValue};

/// Wrapper around the BPF [`HashMap`] for DNS cache operations.
///
/// Provides a safe, typed interface for inserting and looking up cache entries
/// in the eBPF map shared with the XDP program.
pub struct DnsCacheMap {
    inner: HashMap<MapData, CacheKey, CacheValue>,
}

impl DnsCacheMap {
    /// Create a new cache map wrapper from a [`Map`].
    pub fn from_map(map: Map) -> Result<Self> {
        Ok(Self {
            inner: HashMap::try_from(map)?,
        })
    }

    /// Insert or update a cache entry.
    pub fn insert(&mut self, key: &CacheKey, value: &CacheValue) -> Result<()> {
        self.inner.insert(key, value, 0)?;
        Ok(())
    }

    /// Look up a cache entry by key. Returns `None` if not found.
    pub fn get(&mut self, key: &CacheKey) -> Result<Option<CacheValue>> {
        match self.inner.get(key, 0) {
            Ok(value) => Ok(Some(value)),
            Err(_) => Ok(None),
        }
    }
}

/// Return the current kernel monotonic clock time in nanoseconds.
///
/// Uses `clock_gettime(CLOCK_MONOTONIC)` which is compatible with the
/// `bpf_ktime_get_ns()` clock source used in the XDP eBPF program.
pub fn now_ktime_ns() -> u64 {
    // Safety: clock_gettime with CLOCK_MONOTONIC is always available on Linux
    // and writes to a valid timespec struct.
    unsafe {
        let mut ts = core::mem::zeroed::<libc::timespec>();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }
}
