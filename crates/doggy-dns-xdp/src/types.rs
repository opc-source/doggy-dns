/// Must match xdp/doggy-dns-xdp-ebpf/src/types.rs exactly.
pub const FLAG_NEGATIVE: u16 = 0x01;

/// Maximum DNS response size that fits in the eBPF map value.
/// Responses exceeding this are not cached (served via userspace instead).
pub const MAX_RESPONSE_SIZE: usize = 512;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
    /// Spelled out so the key struct is a stable 16 bytes on both sides. The
    /// kernel hashes the QNAME stream only; this field is part of the key but
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

/// Configuration written into the eBPF program's config map before attach.
///
/// This is the channel for settings the program cannot know at compile time;
/// add fields here (in lockstep with `xdp/doggy-dns-xdp-ebpf/src/types.rs`) as
/// the fast path grows new knobs.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct XdpRuntimeConfig {
    /// UDP port the userspace DNS server listens on.
    pub dns_port: u16,
}

/// Per-CPU fast-path counters written by the XDP program.
///
/// Must match `xdp/doggy-dns-xdp-ebpf/src/types.rs` exactly.
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

impl CacheValue {
    /// Pack a response into the map value, truncating to [`MAX_RESPONSE_SIZE`]
    /// and stamping the negative flag when requested.
    fn from_response(resp_data: Vec<u8>, expires_at: u64, negative: bool) -> Self {
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

    pub fn new_positive(resp_data: Vec<u8>, expires_at: u64) -> Self {
        Self::from_response(resp_data, expires_at, false)
    }

    pub fn new_negative(resp_data: Vec<u8>, expires_at: u64) -> Self {
        Self::from_response(resp_data, expires_at, true)
    }

    pub fn is_negative(&self) -> bool {
        self.flags & FLAG_NEGATIVE != 0
    }
}

// Safety: CacheKey and CacheValue are #[repr(C)] with only Pod-compatible types.
// These assertions verify layout at compile time.
const _: () = {
    assert!(core::mem::size_of::<CacheKey>() == 16);
    assert!(core::mem::size_of::<CacheValue>() == 528);
    assert!(core::mem::size_of::<XdpRuntimeConfig>() == 2);
    assert!(core::mem::size_of::<XdpStats>() == 40);
};
