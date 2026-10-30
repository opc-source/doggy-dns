//! Rules for the pinned `DNS_CACHE` map.
//!
//! The loader hands a map to the kernel program *through a bpffs path*
//! (`EbpfLoader::map_pin_path`), which makes a restart continue on the map that
//! is already pinned there instead of failing to create a second one at the
//! same path. Reuse is only safe while the pinned map still fits this build, so
//! the two decisions live here as plain data: where the map goes, and whether
//! an existing one may be used. They are kept out of the Linux-only `program`
//! module because `aya` — and therefore the kernel — is not needed to state or
//! test either rule.

use std::fmt;

use crate::types::{CacheKey, CacheValue};

/// Layout version of the cache entries, written into the pin path.
///
/// A restart reuses the pinned map, so entries survive it. That is the point,
/// but it also means a build whose `CacheKey` or `CacheValue` layout changed
/// would otherwise read entries its predecessor wrote. Bump this constant in
/// the same commit as such a layout change: the next start then creates a fresh
/// (cold) map under the new path rather than misreading the old one. The
/// previous path is left in bpffs and is safe to remove by hand.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// The bpffs path the loader pins and reuses: the configured path with the
/// cache layout version appended, so `/sys/fs/bpf/doggy-dns/dns_cache` holds
/// entries of exactly one layout.
///
/// The version joins with an underscore because bpffs will not pin an object
/// whose name contains a dot — `BPF_OBJ_PIN` fails with `EPERM` — so a `.v1`
/// suffix would turn every start into a startup failure. The configured path
/// inherits the same rule; `validate_config` rejects a `pin_path` with a dot.
pub fn versioned_pin_path(base: &str) -> String {
    format!("{base}_v{CACHE_FORMAT_VERSION}")
}

/// What this build requires of the map behind that path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeclaredCacheShape {
    pub key_size: u32,
    pub value_size: u32,
    /// The capacity the configuration asks for, which a reused map does not
    /// have to agree with.
    pub max_entries: u32,
}

/// Build the requirement from this build's own struct layouts, so a layout
/// change moves `key_size`/`value_size` here without anyone editing a literal.
pub fn declared_cache_shape(max_entries: u32) -> DeclaredCacheShape {
    DeclaredCacheShape {
        key_size: size_of::<CacheKey>() as u32,
        value_size: size_of::<CacheValue>() as u32,
        max_entries,
    }
}

/// What the kernel reports about an already-pinned map.
///
/// `is_lru_hash` is decided by the caller because comparing `aya`'s map type is
/// a Linux-only act; the sizes come from the kernel's own `bpf_map_info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedCacheShape {
    pub is_lru_hash: bool,
    pub key_size: u32,
    pub value_size: u32,
    pub max_entries: u32,
}

/// One way a pinned map fails to fit this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheShapeMismatch {
    /// The map is not an LRU hash, so entries would neither expire the way the
    /// fast path assumes nor be evicted under pressure.
    NotLruHash,
    KeySize {
        actual: u32,
        expected: u32,
    },
    ValueSize {
        actual: u32,
        expected: u32,
    },
    /// The pinned map's capacity is not the configured one, and it wins: the
    /// capacity is fixed when the map is created.
    MaxEntries {
        actual: u32,
        expected: u32,
    },
}

/// Every reason the pinned map does not fit this build, in the fixed order
/// type, key size, value size, capacity. Empty means it fits and can be reused.
///
/// The loader only warns rather than refusing to start: the pinned map is
/// working state that a restarted node has to come up on, and a capacity
/// difference is routine (the operator edited `cache_size`), so failing here
/// would turn an upgrade into an outage. A mismatch does mean the config value
/// in question did not take effect, which is why it is logged.
pub fn cache_shape_mismatch(
    pinned: &PinnedCacheShape,
    declared: &DeclaredCacheShape,
) -> Vec<CacheShapeMismatch> {
    let mut mismatches = Vec::new();
    if !pinned.is_lru_hash {
        mismatches.push(CacheShapeMismatch::NotLruHash);
    }
    if pinned.key_size != declared.key_size {
        mismatches.push(CacheShapeMismatch::KeySize {
            actual: pinned.key_size,
            expected: declared.key_size,
        });
    }
    if pinned.value_size != declared.value_size {
        mismatches.push(CacheShapeMismatch::ValueSize {
            actual: pinned.value_size,
            expected: declared.value_size,
        });
    }
    if pinned.max_entries != declared.max_entries {
        mismatches.push(CacheShapeMismatch::MaxEntries {
            actual: pinned.max_entries,
            expected: declared.max_entries,
        });
    }
    mismatches
}

impl fmt::Display for CacheShapeMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLruHash => write!(formatter, "map type is not LRU hash"),
            Self::KeySize { actual, expected } => {
                write!(formatter, "key size {actual} != {expected}")
            }
            Self::ValueSize { actual, expected } => {
                write!(formatter, "value size {actual} != {expected}")
            }
            Self::MaxEntries { actual, expected } => {
                write!(formatter, "capacity {actual} != configured {expected}")
            }
        }
    }
}
