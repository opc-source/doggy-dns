/// Must match xdp/doggy-dns-xdp-ebpf/src/main.rs exactly.
pub const FLAG_NEGATIVE: u16 = 0x01;

/// Maximum DNS response size that fits in the eBPF map value.
/// Responses exceeding this are not cached (served via userspace instead).
pub const MAX_RESPONSE_SIZE: usize = 512;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
    /// The kernel hashes all key bytes, padding included, so it is spelled out
    /// to keep the value deterministic on both sides.
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
/// add fields here (in lockstep with `xdp/doggy-dns-xdp-ebpf/src/main.rs`) as
/// the fast path grows new knobs.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct XdpRuntimeConfig {
    /// UDP port the userspace DNS server listens on.
    pub dns_port: u16,
}

impl CacheValue {
    pub fn new_positive(resp_data: Vec<u8>, expires_at: u64) -> Self {
        let len = resp_data.len().min(MAX_RESPONSE_SIZE);
        let mut data = [0u8; MAX_RESPONSE_SIZE];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: 0,
            _pad: 0,
            expires_at,
        }
    }

    pub fn new_negative(resp_data: Vec<u8>, expires_at: u64) -> Self {
        let len = resp_data.len().min(MAX_RESPONSE_SIZE);
        let mut data = [0u8; MAX_RESPONSE_SIZE];
        data[..len].copy_from_slice(&resp_data[..len]);
        Self {
            resp_data: data,
            resp_len: len as u16,
            flags: FLAG_NEGATIVE,
            _pad: 0,
            expires_at,
        }
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
};

// aya::Pod is required for using these types as BPF map keys/values.
// Only available on Linux where the `aya` crate is compiled.
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for CacheKey {}
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for CacheValue {}
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for XdpRuntimeConfig {}
