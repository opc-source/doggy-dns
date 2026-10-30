//! Network header layouts and byte-level packet helpers.
//!
//! aya-ebpf-bindings does not export usable header definitions, so the three
//! structs here describe the on-wire layout we parse. The `read`/`write`/
//! `swap`/`copy` helpers take raw packet pointers and perform NO bounds
//! checking: every caller must prove `ptr + len <= data_end` first, exactly
//! as the verifier-oriented comments in `main.rs` describe.

pub(crate) const ETH_P_IP: u16 = 0x0800;
pub(crate) const IPPROTO_UDP: u8 = 17;

pub(crate) const ETH_HDR_LEN: usize = core::mem::size_of::<EthHdr>();
pub(crate) const UDP_HDR_LEN: usize = core::mem::size_of::<UdpHdr>();

/// Chunk size used when copying cached responses into the frame. See
/// [`copy_chunk`] for why this shape matters to the verifier.
pub(crate) const COPY_CHUNK: usize = 16;

#[repr(C)]
pub(crate) struct EthHdr {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub h_proto: u16,
}

#[repr(C)]
pub(crate) struct IpHdr {
    pub ihl_ver: u8, // version (4 bits) + IHL (4 bits)
    pub tos: u8,
    pub tot_len: u16,
    pub id: u16,
    pub frag_off: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub check: u16,
    pub saddr: u32,
    pub daddr: u32,
}

impl IpHdr {
    #[inline]
    pub(crate) fn ihl(&self) -> u8 {
        self.ihl_ver & 0x0F
    }
}

#[repr(C)]
pub(crate) struct UdpHdr {
    pub source: u16,
    pub dest: u16,
    pub len: u16,
    pub check: u16,
}

/// Copy [`COPY_CHUNK`] bytes, all at constant offsets from `dst`.
///
/// Two 64-bit moves instead of sixteen byte moves: the caller's one
/// `dst + COPY_CHUNK > data_end` check still proves every byte written, and a
/// quarter as many instructions hit the verifier and the CPU. Plain derefs,
/// not `read_unaligned`/`write_unaligned`: LLVM turns those into byte-by-byte
/// accesses, while the verifier is happy with unaligned loads and stores on
/// packet data and map values.
#[inline(always)]
pub(crate) fn copy_chunk(dst: usize, src: *const u8, src_off: usize) {
    unsafe {
        let lo = *(src.add(src_off) as *const u64);
        let hi = *(src.add(src_off + 8) as *const u64);
        *(dst as *mut u64) = lo;
        *((dst + 8) as *mut u64) = hi;
    }
}

#[inline(always)]
pub(crate) fn swap_u16(p: *mut u8, a: usize, b: usize) {
    unsafe {
        let x = *(p.add(a) as *const u16);
        let y = *(p.add(b) as *const u16);
        *(p.add(a) as *mut u16) = y;
        *(p.add(b) as *mut u16) = x;
    }
}

#[inline(always)]
pub(crate) fn swap_u32(p: *mut u8, a: usize, b: usize) {
    unsafe {
        let x = *(p.add(a) as *const u32);
        let y = *(p.add(b) as *const u32);
        *(p.add(a) as *mut u32) = y;
        *(p.add(b) as *mut u32) = x;
    }
}

#[inline]
pub(crate) fn read_u16_be(ptr: *const u8, offset: usize) -> u16 {
    unsafe {
        let hi = *ptr.add(offset) as u16;
        let lo = *ptr.add(offset + 1) as u16;
        (hi << 8) | lo
    }
}

#[inline]
pub(crate) fn write_u16_be(ptr: *mut u8, offset: usize, val: u16) {
    unsafe {
        *ptr.add(offset) = (val >> 8) as u8;
        *ptr.add(offset + 1) = (val & 0xFF) as u8;
    }
}
