use crate::types::CacheKey;
#[cfg(target_os = "linux")]
use crate::types::CacheValue;

/// FNV-1a 64-bit hash. Used to hash DNS QNAME for BPF map keys.
///
/// This is a non-cryptographic hash function that is fast and has good
/// distribution properties, making it suitable for hash table keys in BPF.
pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Build a [`CacheKey`] from DNS question name, type, and class.
///
/// The QNAME is lowercased before hashing to ensure case-insensitive matching,
/// consistent with DNS semantics.
pub fn make_cache_key(qname: &str, qtype: u16, qclass: u16) -> CacheKey {
    CacheKey {
        name_hash: fnv1a_64(qname.to_ascii_lowercase().as_bytes()),
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
    }
}

// ---------------------------------------------------------------------------
// Linux-only: BPF map wrapper and kernel-monotonic clock helper
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
use anyhow::Result;
#[cfg(target_os = "linux")]
use aya::maps::{HashMap, Map, MapData};

/// Wrapper around the BPF [`HashMap`] for DNS cache operations.
///
/// Provides a safe, typed interface for inserting and looking up cache entries
/// in the eBPF map shared with the XDP program.
#[cfg(target_os = "linux")]
pub struct DnsCacheMap {
    inner: HashMap<MapData, CacheKey, CacheValue>,
}

#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
pub fn now_ktime_ns() -> u64 {
    // Safety: clock_gettime with CLOCK_MONOTONIC is always available on Linux
    // and writes to a valid timespec struct.
    unsafe {
        let mut ts = core::mem::zeroed::<libc::timespec>();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
    }
}
