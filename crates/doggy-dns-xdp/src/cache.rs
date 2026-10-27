use crate::types::CacheKey;
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
//
// Lives in a gated submodule so `aya` code never needs item-level cfg
// attributes, and the key-hashing code above builds on every platform.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub mod bpf_map;
#[cfg(target_os = "linux")]
pub use bpf_map::{DnsCacheMap, now_ktime_ns};
