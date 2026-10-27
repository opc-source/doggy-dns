//! `aya::Pod` implementations for the shared BPF layout types (Linux only).
//!
//! `aya` is only compiled on Linux, so this module is gated once in `lib.rs`
//! instead of tagging every impl. The type definitions and their compile-time
//! size assertions stay in [`crate::types`] so they build on every platform.

use crate::types::{CacheKey, CacheValue, XdpRuntimeConfig, XdpStats};

// Safety: these types are #[repr(C)] with only Pod-compatible fields, which
// the size assertions in `types.rs` pin down at compile time.
unsafe impl aya::Pod for CacheKey {}
unsafe impl aya::Pod for CacheValue {}
unsafe impl aya::Pod for XdpRuntimeConfig {}
unsafe impl aya::Pod for XdpStats {}
