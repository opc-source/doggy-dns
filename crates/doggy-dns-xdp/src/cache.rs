use crate::types::CacheKey;
#[cfg(target_os = "linux")]
use crate::types::CacheValue;
use hickory_proto::rr::Name;

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
/// The name is hashed in wire form — each label prefixed with its length and
/// terminated by the root label — with the label bytes lowercased for
/// case-insensitive matching. The eBPF parser hashes the packet's QNAME bytes
/// exactly the same way; the length prefixes keep the encoding injective, so
/// `ab.c` and `a.bc` hash differently.
pub fn make_cache_key(name: &Name, qtype: u16, qclass: u16) -> CacheKey {
    let mut hash: u64 = 0xcbf29ce484222325;
    for label in name.iter() {
        hash ^= label.len() as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        for &byte in label {
            hash ^= byte.to_ascii_lowercase() as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    // The root label's zero byte: xor with zero is a no-op.
    hash = hash.wrapping_mul(0x100000001b3);

    CacheKey {
        name_hash: hash,
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
        _pad: 0,
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
