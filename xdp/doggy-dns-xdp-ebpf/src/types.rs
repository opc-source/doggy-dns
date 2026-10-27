//! BPF map layout types — must match `crates/doggy-dns-xdp/src/types.rs`
//! exactly.
//!
//! These structs are hand-mirrored across the `no_std` boundary by project
//! convention: change one side, change the other in the same commit. The
//! compile-time size assertion below is the drift guard for `XdpStats`,
//! mirroring the userspace assertion set.

pub const FLAG_NEGATIVE: u16 = 0x01;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
    // The kernel hashes all key bytes, padding included, so it is spelled out
    // to keep the value deterministic on both sides.
    pub _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheValue {
    pub resp_data: [u8; 512],
    pub resp_len: u16,
    pub flags: u16,
    pub _pad: u32,
    pub expires_at: u64,
}

/// Configuration written by the userspace loader before the program is
/// attached. Must match crates/doggy-dns-xdp/src/types.rs exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct XdpRuntimeConfig {
    pub dns_port: u16,
}

/// Per-CPU fast-path counters. Must match crates/doggy-dns-xdp/src/types.rs
/// exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct XdpStats {
    pub hit: u64,
    pub negative_hit: u64,
    pub miss: u64,
    pub expired: u64,
    pub drop: u64,
}

const _: () = {
    assert!(core::mem::size_of::<XdpStats>() == 40);
};
