//! Rules for the pinned `DNS_CACHE` map path and for reusing a map that
//! already exists.
//!
//! These live in the cross-platform `pin` module rather than inside the
//! Linux-only `program` module so the rules themselves can be tested on any
//! host; `program.rs` only feeds them what `aya` reports about a real map.

#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::pin::{
    CACHE_FORMAT_VERSION, CacheShapeMismatch, PinnedCacheShape, cache_shape_mismatch,
    declared_cache_shape, versioned_pin_path,
};
use doggy_dns_xdp::types::{CacheKey, CacheValue};

/// The pin path carries the cache layout version, so a `CacheValue` change
/// makes the next start create a fresh map instead of handing the new program
/// entries laid out under the old format.
///
/// The separator is an underscore, not a dot: bpffs refuses to pin an object
/// whose name contains a dot, and the loader would fail with `EPERM` at
/// startup (`BPF_OBJ_PIN failed: Operation not permitted`).
#[test]
fn pin_path_carries_the_cache_format_version() {
    assert_eq!(
        versioned_pin_path("/sys/fs/bpf/doggy-dns/dns_cache"),
        format!("/sys/fs/bpf/doggy-dns/dns_cache_v{CACHE_FORMAT_VERSION}")
    );
}

/// The version suffix is only safe while it adds no character bpffs rejects,
/// and the path it returns stays pinnable for a path that already is.
#[test]
fn pin_path_stays_pinnable() {
    let versioned = versioned_pin_path("/sys/fs/bpf/doggy-dns/dns_cache");
    assert!(
        !versioned.contains('.'),
        "bpffs will not pin a name containing a dot: {versioned}"
    );
    assert!(
        versioned.starts_with("/sys/fs/bpf/"),
        "the version suffix must not move the path out of bpffs: {versioned}"
    );
}

/// The first release that reuses pinned maps has no predecessor to stay
/// compatible with, so version 1 is the baseline: bumping it is a deliberate
/// act that also strands the previous path, which is the point.
#[test]
fn cache_format_version_starts_at_one() {
    assert_eq!(CACHE_FORMAT_VERSION, 1);
}

/// The shape a pinned map must have to be reusable is the shape this build's
/// own structs declare, with the configured capacity on top.
#[test]
fn declared_shape_comes_from_the_shared_structs() {
    let declared = declared_cache_shape(4096);
    assert_eq!(declared.key_size, size_of::<CacheKey>() as u32);
    assert_eq!(declared.value_size, size_of::<CacheValue>() as u32);
    assert_eq!(declared.max_entries, 4096);
}

/// A map that matches on every count is reusable, which is the ordinary
/// restart case.
#[test]
fn matching_shape_reports_no_mismatch() {
    let declared = declared_cache_shape(1024);
    let pinned = PinnedCacheShape {
        is_lru_hash: true,
        key_size: declared.key_size,
        value_size: declared.value_size,
        max_entries: declared.max_entries,
    };
    assert!(cache_shape_mismatch(&pinned, &declared).is_empty());
}

/// Every way a pinned map can fail to fit this build has to be named, because
/// the loader only warns: an operator reading the log must learn which number
/// stopped matching and that the configured capacity therefore did not take
/// effect.
#[test]
fn mismatches_name_the_field_that_stops_matching() {
    let declared = declared_cache_shape(1024);

    let wrong_type = PinnedCacheShape {
        is_lru_hash: false,
        key_size: declared.key_size,
        value_size: declared.value_size,
        max_entries: declared.max_entries,
    };
    assert_eq!(
        cache_shape_mismatch(&wrong_type, &declared),
        vec![CacheShapeMismatch::NotLruHash]
    );

    let wrong_key = PinnedCacheShape {
        is_lru_hash: true,
        key_size: declared.key_size + 8,
        value_size: declared.value_size,
        max_entries: declared.max_entries,
    };
    assert_eq!(
        cache_shape_mismatch(&wrong_key, &declared),
        vec![CacheShapeMismatch::KeySize {
            actual: declared.key_size + 8,
            expected: declared.key_size,
        }]
    );

    let wrong_value = PinnedCacheShape {
        is_lru_hash: true,
        key_size: declared.key_size,
        value_size: 999,
        max_entries: declared.max_entries,
    };
    assert_eq!(
        cache_shape_mismatch(&wrong_value, &declared),
        vec![CacheShapeMismatch::ValueSize {
            actual: 999,
            expected: declared.value_size,
        }]
    );

    let wrong_capacity = PinnedCacheShape {
        is_lru_hash: true,
        key_size: declared.key_size,
        value_size: declared.value_size,
        max_entries: 64,
    };
    assert_eq!(
        cache_shape_mismatch(&wrong_capacity, &declared),
        vec![CacheShapeMismatch::MaxEntries {
            actual: 64,
            expected: 1024,
        }]
    );
}

/// Several differences at once are all reported, in the fixed order
/// type, key size, value size, capacity, so the log is stable across runs
/// rather than depending on which check happened to be written first.
#[test]
fn every_mismatch_is_listed_in_a_fixed_order() {
    let declared = declared_cache_shape(1024);
    let pinned = PinnedCacheShape {
        is_lru_hash: false,
        key_size: declared.key_size + 1,
        value_size: declared.value_size + 1,
        max_entries: declared.max_entries - 1,
    };
    assert_eq!(
        cache_shape_mismatch(&pinned, &declared),
        vec![
            CacheShapeMismatch::NotLruHash,
            CacheShapeMismatch::KeySize {
                actual: declared.key_size + 1,
                expected: declared.key_size,
            },
            CacheShapeMismatch::ValueSize {
                actual: declared.value_size + 1,
                expected: declared.value_size,
            },
            CacheShapeMismatch::MaxEntries {
                actual: declared.max_entries - 1,
                expected: declared.max_entries,
            },
        ]
    );
}
