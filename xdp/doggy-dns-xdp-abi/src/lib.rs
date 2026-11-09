//! BPF map ABI shared by the XDP program and its userspace loader.
//!
//! `xdp/doggy-dns-xdp-ebpf` and `crates/doggy-dns-xdp` compile these
//! definitions from one source: the structs are raw byte images in BPF maps,
//! so a change here changes both sides at once.
//!
//! The crate must stay `no_std`, and the eBPF build (`bpfel-unknown-none`,
//! `-Z build-std=core,alloc`) must stay free of runtime dependencies. The
//! `aya::Pod` impls below are gated on `target_os = "linux"`, the only
//! platform with a userspace loader, so the eBPF target never pulls `aya`.

#![no_std]

/// Cache-value flag: the entry holds an NXDOMAIN answer.
pub const FLAG_NEGATIVE: u16 = 0x01;

/// Largest DNS response that fits in the cache value.
///
/// Responses exceeding this are not cached (served via userspace instead), and
/// the eBPF program refuses to copy a cached reply above it.
pub const MAX_RESPONSE_SIZE: usize = 512;

/// Layout version of the cache entries, written into the pin path.
///
/// A restart reuses the pinned map, so entries survive it. That is the point,
/// but it also means a build whose `CacheKey` or `CacheValue` layout changed
/// would otherwise read entries its predecessor wrote. Bump this constant in
/// the same commit as such a layout change: the next start then creates a
/// fresh (cold) map under the new path rather than misreading the old one. The
/// previous path is left in bpffs and is safe to remove by hand.
pub const CACHE_FORMAT_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
    /// Spelled out so the key struct is a stable 16 bytes on both sides. The
    /// hash covers the QNAME stream only; this field is part of the key but
    /// never of the hash.
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheValue {
    pub resp_data: [u8; MAX_RESPONSE_SIZE],
    pub resp_len: u16,
    pub flags: u16,
    pub _pad: u32,
    pub expires_at: u64,
}

impl CacheValue {
    /// Pack a response into the map value, truncating to [`MAX_RESPONSE_SIZE`]
    /// and stamping the negative flag when requested.
    fn from_response(resp_data: &[u8], expires_at: u64, negative: bool) -> Self {
        let len = resp_data.len().min(MAX_RESPONSE_SIZE);
        let mut data = [0u8; MAX_RESPONSE_SIZE];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: if negative { FLAG_NEGATIVE } else { 0 },
            _pad: 0,
            expires_at,
        }
    }

    pub fn new_positive(resp_data: &[u8], expires_at: u64) -> Self {
        Self::from_response(resp_data, expires_at, false)
    }

    pub fn new_negative(resp_data: &[u8], expires_at: u64) -> Self {
        Self::from_response(resp_data, expires_at, true)
    }
}

/// Configuration written into the eBPF program's config map before attach.
///
/// This is the channel for settings the program cannot know at compile time.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct XdpRuntimeConfig {
    /// UDP port the userspace DNS server listens on.
    pub dns_port: u16,
}

/// Per-CPU fast-path counters written by the XDP program.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XdpStats {
    /// Queries answered from the cache (`XDP_TX`, positive entry).
    pub hit: u64,
    /// Queries answered from the cache with a cached NXDOMAIN.
    pub negative_hit: u64,
    /// Queries whose key was absent from the cache.
    pub miss: u64,
    /// Queries whose key was present but already expired.
    pub expired: u64,
    /// Packets dropped after the frame was resized (rewrite failure).
    pub drop: u64,
}

// Safety: each layout is `#[repr(C)]` with no padding — the size and alignment
// assertions below pin that down at compile time — so every initialized byte
// sequence of the right size is a valid value, as `aya::Pod` requires.
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for CacheKey {}
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for CacheValue {}
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for XdpRuntimeConfig {}
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for XdpStats {}

const _: () = {
    // Literal sizes, not expressions over the constants: a pinned map keeps
    // its `value_size`, so changing `MAX_RESPONSE_SIZE` must trip a compile
    // error here and force a `CACHE_FORMAT_VERSION` bump too.
    assert!(core::mem::size_of::<CacheKey>() == 16);
    assert!(core::mem::size_of::<CacheValue>() == 528);
    assert!(core::mem::size_of::<XdpRuntimeConfig>() == 2);
    assert!(core::mem::size_of::<XdpStats>() == 40);
    assert!(core::mem::align_of::<CacheKey>() == 8);
    assert!(core::mem::align_of::<CacheValue>() == 8);
    assert!(core::mem::align_of::<XdpRuntimeConfig>() == 2);
    assert!(core::mem::align_of::<XdpStats>() == 8);
};
