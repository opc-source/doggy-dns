#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    macros::{map, xdp},
    maps::LruHashMap,
    programs::XdpContext,
};
use aya_log_ebpf::info;

// Shared types — must match crates/doggy-dns-xdp/src/types.rs exactly
pub const FLAG_NEGATIVE: u16 = 0x01;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CacheKey {
    pub name_hash: u64,
    pub qtype_class: u32,
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

#[map]
static DNS_CACHE: LruHashMap<CacheKey, CacheValue> =
    LruHashMap::with_max_entries(65536, 0);

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    match try_dns_xdp(ctx) {
        Ok(action) => action,
        Err(_) => xdp_action::XDP_PASS,
    }
}

fn try_dns_xdp(ctx: XdpContext) -> Result<u32, ()> {
    // Task 5 will implement this
    let _ = &info!;
    let _ = &DNS_CACHE;
    Ok(xdp_action::XDP_PASS)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
