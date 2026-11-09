//! BPF map layouts, shared with the eBPF program through `doggy-dns-xdp-abi`.
//!
//! The definitions live in `xdp/doggy-dns-xdp-abi`, which the kernel-side
//! program compiles too, so the two sides cannot drift apart.

pub use doggy_dns_xdp_abi::{
    CacheKey, CacheValue, FLAG_NEGATIVE, MAX_RESPONSE_SIZE, XdpRuntimeConfig, XdpStats,
};
