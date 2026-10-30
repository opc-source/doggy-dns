use std::mem::MaybeUninit;
use std::slice;

use crate::types::CacheKey;
use hickory_proto::rr::Name;

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// One FNV-1a step over a little-endian word.
#[inline]
fn fnv1a_word(hash: u64, word: u64) -> u64 {
    (hash ^ word).wrapping_mul(FNV_PRIME)
}

/// FNV-1a 64-bit hash over zero-padded little-endian words.
///
/// The input is padded with zero bytes to a multiple of eight and hashed one
/// little-endian `u64` word at a time: `hash = (hash ^ word) * prime`. That is
/// one multiply per eight bytes instead of one per byte, and it is the only
/// shape the eBPF side can mirror with a fixed number of instructions.
///
/// `fnv1a_64(b"a")` still equals the classic byte-wise FNV-1a of `"a"`: a
/// single byte is a single word whose value is the byte.
pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    let (chunks, tail) = data.as_chunks::<8>();
    for chunk in chunks {
        hash = fnv1a_word(hash, u64::from_le_bytes(*chunk));
    }
    if !tail.is_empty() {
        let mut padded = [0u8; 8];
        padded[..tail.len()].copy_from_slice(tail);
        hash = fnv1a_word(hash, u64::from_le_bytes(padded));
    }
    hash
}

/// Build a [`CacheKey`] from DNS question name, type, and class.
///
/// The name is hashed in wire form — each label prefixed with its length and
/// terminated by the root label — with the label bytes lowercased for
/// case-insensitive matching. The eBPF parser hashes the packet's QNAME bytes
/// exactly the same way, eight at a time; the length prefixes keep the
/// encoding injective, so `ab.c` and `a.bc` hash differently.
pub fn make_cache_key(name: &Name, qtype: u16, qclass: u16) -> CacheKey {
    // hickory bounds the encoded length on both construction paths: appends go
    // through `extend_name`, which rejects anything past `Name::MAX_LENGTH`
    // (255, terminator included), and wire parsing rejects a name once its
    // encoded length reaches 255. The wire form below therefore always fits:
    // no allocation, no length check.
    //
    // Uninitialised on purpose: zeroing ~256 bytes costs more than the hash
    // itself on short names, and every byte below `pos` is written before
    // `fnv1a_64` reads the slice.
    let mut storage = MaybeUninit::<[u8; Name::MAX_LENGTH + 2]>::uninit();
    let wire = storage.as_mut_ptr().cast::<u8>();
    let mut pos = 0;
    for label in name.iter() {
        unsafe { *wire.add(pos) = label.len() as u8 };
        pos += 1;
        for &byte in label {
            unsafe { *wire.add(pos) = byte.to_ascii_lowercase() };
            pos += 1;
        }
    }
    // Root label's zero byte, part of the hashed stream.
    unsafe { *wire.add(pos) = 0 };
    pos += 1;

    // `Name::len()` is the encoded wire length and includes that terminator,
    // so any break of the bound would be a hickory invariant change, not a
    // bad caller: fail loudly in tests instead of writing past `storage`.
    debug_assert!(pos <= Name::MAX_LENGTH, "wire name {pos} bytes");

    // Safety: bytes `0..pos` are all initialised above, `pos <= Name::MAX_LENGTH`
    // by the invariant just asserted, and `storage` outlives the borrow.
    let wire = unsafe { slice::from_raw_parts(wire, pos) };

    CacheKey {
        name_hash: fnv1a_64(wire),
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
