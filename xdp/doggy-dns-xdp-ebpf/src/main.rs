#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::{bpf_ktime_get_ns, bpf_xdp_adjust_tail},
    macros::{map, xdp},
    maps::{Array, LruHashMap},
    programs::XdpContext,
};

// ---------------------------------------------------------------------------
// Shared types — must match crates/doggy-dns-xdp/src/types.rs exactly
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Network header structs (aya-ebpf-bindings does not export usable definitions)
// ---------------------------------------------------------------------------

#[repr(C)]
struct EthHdr {
    dst: [u8; 6],
    src: [u8; 6],
    h_proto: u16,
}

#[repr(C)]
struct IpHdr {
    ihl_ver: u8, // version (4 bits) + IHL (4 bits)
    tos: u8,
    tot_len: u16,
    id: u16,
    frag_off: u16,
    ttl: u8,
    protocol: u8,
    check: u16,
    saddr: u32,
    daddr: u32,
}

impl IpHdr {
    #[inline]
    fn ihl(&self) -> u8 {
        self.ihl_ver & 0x0F
    }
}

#[repr(C)]
struct UdpHdr {
    source: u16,
    dest: u16,
    len: u16,
    check: u16,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ETH_P_IP: u16 = 0x0800;
const IPPROTO_UDP: u8 = 17;
// A QNAME is at most 255 bytes on the wire, including the root label.
const MAX_QNAME_LEN: usize = 255;
const ETH_HDR_LEN: usize = core::mem::size_of::<EthHdr>();
const UDP_HDR_LEN: usize = core::mem::size_of::<UdpHdr>();
const DNS_HDR_LEN: usize = 12;
const MAX_DNS_LEN: usize = 512;
const COPY_CHUNK: usize = 4;

// ---------------------------------------------------------------------------
// BPF map
// ---------------------------------------------------------------------------

#[map]
static DNS_CACHE: LruHashMap<CacheKey, CacheValue> =
    LruHashMap::with_max_entries(65536, 0);

/// Single-entry map holding the settings the userspace loader supplies.
#[map]
static XDP_CONFIG: Array<XdpRuntimeConfig> = Array::with_max_entries(1, 0);

// ---------------------------------------------------------------------------
// XDP entry point
// ---------------------------------------------------------------------------

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    match try_dns_xdp(&ctx) {
        Ok(action) => action,
        // Errors only happen after the frame has been resized, so the packet
        // can no longer be handed on as the original query.
        Err(_) => xdp_action::XDP_DROP,
    }
}

fn try_dns_xdp(ctx: &XdpContext) -> Result<u32, ()> {
    let data = ctx.data();
    let end = ctx.data_end();

    // 1. Parse Ethernet header
    if data + ETH_HDR_LEN > end {
        return Ok(xdp_action::XDP_PASS);
    }
    let eth = unsafe { &*(data as *const EthHdr) };
    if u16::from_be(eth.h_proto) != ETH_P_IP {
        return Ok(xdp_action::XDP_PASS);
    }

    // 2. Parse IPv4 header
    let ip_offset = ETH_HDR_LEN;
    if data + ip_offset + core::mem::size_of::<IpHdr>() > end {
        return Ok(xdp_action::XDP_PASS);
    }
    let ip = unsafe { &*((data + ip_offset) as *const IpHdr) };
    if ip.protocol != IPPROTO_UDP {
        return Ok(xdp_action::XDP_PASS);
    }
    let ip_hdr_len = (ip.ihl() as usize) * 4;
    if ip_hdr_len < 20 || data + ip_offset + ip_hdr_len > end {
        return Ok(xdp_action::XDP_PASS);
    }

    // 3. Parse UDP header
    let udp_offset = ip_offset + ip_hdr_len;
    if data + udp_offset + UDP_HDR_LEN > end {
        return Ok(xdp_action::XDP_PASS);
    }
    let udp = unsafe { &*((data + udp_offset) as *const UdpHdr) };
    // The userspace server may listen on any port, so it is read from the
    // configuration map the loader fills in before attaching.
    let Some(config) = XDP_CONFIG.get(0) else {
        return Ok(xdp_action::XDP_PASS);
    };
    if u16::from_be(udp.dest) != config.dns_port {
        return Ok(xdp_action::XDP_PASS);
    }
    let udp_len = u16::from_be(udp.len) as usize;
    if udp_len < UDP_HDR_LEN {
        return Ok(xdp_action::XDP_PASS);
    }
    // Current DNS message length, taken from the UDP length field. The packet
    // length cannot be derived from `data_end - data`: the verifier rejects any
    // arithmetic involving a PTR_TO_PACKET_END register.
    let old_dns_len = udp_len - UDP_HDR_LEN;

    // 4. Parse DNS header
    let dns_offset = udp_offset + UDP_HDR_LEN;
    if data + dns_offset + DNS_HDR_LEN > end {
        return Ok(xdp_action::XDP_PASS);
    }
    let dns_ptr = (data + dns_offset) as *const u8;

    // Query must have QR=0 and QDCOUNT=1
    let flags = read_u16_be(dns_ptr, 2);
    if flags & 0x8000 != 0 || read_u16_be(dns_ptr, 4) != 1 {
        return Ok(xdp_action::XDP_PASS);
    }

    // 5. Extract QNAME and compute hash
    let (name_hash, qtype, qclass, question_len) = match parse_dns_question(ctx, dns_offset) {
        Some(v) => v,
        None => return Ok(xdp_action::XDP_PASS),
    };

    let key = CacheKey {
        name_hash,
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
        _pad: 0,
    };

    // 6. Cache lookup
    let entry = match unsafe { DNS_CACHE.get(&key) } {
        Some(entry) => entry,
        None => return Ok(xdp_action::XDP_PASS),
    };

    // 7. TTL check
    let now = unsafe { bpf_ktime_get_ns() };
    if entry.expires_at <= now {
        return Ok(xdp_action::XDP_PASS);
    }

    // The query must be fully contained in the frame before we resize it.
    if data + dns_offset + old_dns_len > end {
        return Ok(xdp_action::XDP_PASS);
    }

    let txid = read_u16_be(dns_ptr, 0);
    let negative = entry.flags & FLAG_NEGATIVE != 0;
    let resp_len = entry.resp_len as usize;
    let new_dns_len = if negative {
        DNS_HDR_LEN + question_len
    } else {
        if resp_len < DNS_HDR_LEN || resp_len > MAX_DNS_LEN {
            return Ok(xdp_action::XDP_PASS);
        }
        resp_len
    };

    // 8. Resize the frame to the new DNS message length.
    let delta = new_dns_len as i32 - old_dns_len as i32;
    if unsafe { bpf_xdp_adjust_tail(ctx.ctx, delta) } != 0 {
        return Ok(xdp_action::XDP_PASS);
    }

    // Past this point the frame has been resized, so every failure returns
    // `Err` and the packet is dropped instead of being passed on: the original
    // query no longer exists.
    let data = ctx.data();
    let end = ctx.data_end();
    if data + dns_offset + new_dns_len > end {
        return Err(());
    }
    let dns = data + dns_offset;

    if negative {
        write_nxdomain(dns, end)?;
    } else {
        write_positive(dns, end, entry, resp_len, txid)?;
    }

    // 9. Swap headers for reply, fix lengths, and recompute checksums.
    finalize_headers(ctx, ip_hdr_len, udp_offset, new_dns_len)?;

    Ok(xdp_action::XDP_TX)
}

// ---------------------------------------------------------------------------
// DNS question parsing + FNV-1a hash
// ---------------------------------------------------------------------------

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn parse_dns_question(
    ctx: &XdpContext,
    dns_offset: usize,
) -> Option<(u64, u16, u16, usize)> {
    let data = ctx.data();
    let end = ctx.data_end();
    // The QNAME is hashed as one flat stream of wire bytes (length-prefixed
    // labels, lowercased), matching the userspace key builder. A per-label
    // loop does not fit the verifier budget: every label length re-splits the
    // path, so the explored state count grows with each nesting level.
    let mut hash = FNV_OFFSET_BASIS;
    let mut pos = dns_offset + DNS_HDR_LEN;
    let mut name_len = 0usize;

    loop {
        if name_len >= MAX_QNAME_LEN {
            return None;
        }
        // Compare packet-relative addresses, not bare offsets: the verifier
        // only tracks bounds for pointers derived from the packet. The check
        // asks for two bytes because on a pointer with a zero fixed offset a
        // right-open range grants nothing, and the constant addend also keeps
        // the compiler from folding the comparison to `end > cur` (which moves
        // the proven range to the wrong branch).
        let cur = data + pos;
        if cur + 2 > end {
            return None;
        }
        let byte = unsafe { *(cur as *const u8) };
        // Length bytes and the root's zero are below 'A', so lowercasing them
        // is an identity and the whole name can be folded uniformly.
        hash ^= byte.to_ascii_lowercase() as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        pos += 1;
        name_len += 1;
        if byte == 0 {
            break; // Root label — end of QNAME
        }
    }

    // QTYPE follows the QNAME (pos is already past the root label). Read both
    // fields byte-by-byte to avoid misaligned reads.
    let q_ptr = data + pos;
    if q_ptr + 4 > end {
        return None;
    }
    let qtype = unsafe {
        let p = q_ptr as *const u8;
        (u16::from(*p) << 8) | u16::from(*p.add(1))
    };
    let qclass = unsafe {
        let p = q_ptr as *const u8;
        (u16::from(*p.add(2)) << 8) | u16::from(*p.add(3))
    };

    // Length of the question section itself (QNAME + QTYPE + QCLASS), which
    // starts right after the DNS header. `name_len` counts the QNAME's wire
    // bytes, so this stays a bounded scalar instead of a pointer difference.
    let question_len = name_len + 4;

    Some((hash, qtype, qclass, question_len))
}

// ---------------------------------------------------------------------------
// Response construction
// ---------------------------------------------------------------------------

/// Rewrite the DNS header for a cached NXDOMAIN answer.
#[inline(always)]
fn write_nxdomain(dns: usize, end: usize) -> Result<(), ()> {
    // A constant offset keeps the packet register's id, so the range proven by
    // this comparison also covers every access below.
    if dns + DNS_HDR_LEN > end {
        return Err(());
    }

    // Keep the RD bit from the query, set QR=1 and RCODE=3 (NXDomain).
    let orig_flags = read_u16_be(dns as *const u8, 2);
    let new_flags = 0x8000 | (orig_flags & 0x0100) | 0x0003;
    write_u16_be(dns as *mut u8, 2, new_flags);

    // ANCOUNT, NSCOUNT, ARCOUNT = 0
    write_u16_be(dns as *mut u8, 6, 0);
    write_u16_be(dns as *mut u8, 8, 0);
    write_u16_be(dns as *mut u8, 10, 0);

    Ok(())
}

/// Copy the cached reply into the packet, keeping the query's transaction id.
///
/// The copy runs in [`COPY_CHUNK`]-sized pieces, each guarded by a
/// `p + COPY_CHUNK > end` comparison. The constant addend is what makes the
/// guard useful to the verifier: it derives a packet range from the pointer's
/// fixed offset, and a chunk larger than one byte keeps the compiler from
/// folding the comparison into the rangeless `p >= end` form.
#[inline(always)]
fn write_positive(
    dns: usize,
    end: usize,
    entry: &CacheValue,
    resp_len: usize,
    txid: u16,
) -> Result<(), ()> {
    if resp_len < DNS_HDR_LEN || resp_len > MAX_DNS_LEN {
        return Err(());
    }

    let src = entry.resp_data.as_ptr();
    // Copy the leading chunk so that the loop below is aligned to it and ends
    // exactly at resp_len. resp_len >= DNS_HDR_LEN, so four bytes are always
    // available.
    let lead = resp_len & (COPY_CHUNK - 1);
    let mut i = lead;
    if lead != 0 {
        if dns + COPY_CHUNK > end {
            return Err(());
        }
        copy_chunk(dns, src, 0);
    }
    while i < resp_len {
        let p = dns + i;
        if p + COPY_CHUNK > end {
            return Err(());
        }
        copy_chunk(p, src, i);
        i += COPY_CHUNK;
    }

    // The cached reply carries the transaction id of the query that populated
    // the cache; overwrite it with the current query's id.
    if dns + 2 > end {
        return Err(());
    }
    write_u16_be(dns as *mut u8, 0, txid);

    Ok(())
}

// ---------------------------------------------------------------------------
// Header swap + length fixups + checksum recalculation
// ---------------------------------------------------------------------------

#[inline(always)]
fn finalize_headers(
    ctx: &XdpContext,
    ip_hdr_len: usize,
    udp_offset: usize,
    new_dns_len: usize,
) -> Result<(), ()> {
    let data = ctx.data();
    let end = ctx.data_end();

    // Ethernet: swap source and destination MAC addresses. Every access below
    // has a constant offset, so this single check covers all of them.
    if data + ETH_HDR_LEN > end {
        return Err(());
    }
    let eth = data as *mut u8;
    swap_u8(eth, 0, 6);
    swap_u8(eth, 1, 7);
    swap_u8(eth, 2, 8);
    swap_u8(eth, 3, 9);
    swap_u8(eth, 4, 10);
    swap_u8(eth, 5, 11);

    // IPv4: swap addresses, set DF, fix the total length and update the header
    // checksum. The checksum is updated incrementally (RFC 1624) rather than
    // recomputed over the whole header: a header walk needs a variable offset,
    // and the verifier only accepts variable offsets that are re-checked
    // against data_end on every iteration.
    if data + ETH_HDR_LEN + core::mem::size_of::<IpHdr>() > end {
        return Err(());
    }
    let ip = (data + ETH_HDR_LEN) as *mut u8;
    let old_tot_len = read_u16_be(ip, 2);
    let old_frag_off = read_u16_be(ip, 6);
    let old_check = read_u16_be(ip, 10);

    // Swapping the two addresses leaves the one's complement sum unchanged.
    swap_u8(ip, 12, 16);
    swap_u8(ip, 13, 17);
    swap_u8(ip, 14, 18);
    swap_u8(ip, 15, 19);

    let new_tot_len = (ip_hdr_len + UDP_HDR_LEN + new_dns_len) as u16;
    let new_frag_off = old_frag_off | 0x4000; // Don't Fragment
    write_u16_be(ip, 2, new_tot_len);
    write_u16_be(ip, 6, new_frag_off);

    // HC' = ~(~HC + ~m + m') over the two words that changed (RFC 1624 §3).
    let mut sum = (!old_check) as u32
        + (!old_tot_len) as u32
        + new_tot_len as u32
        + (!old_frag_off) as u32
        + new_frag_off as u32;
    // Five addends of at most 16 bits each, so two folds are enough.
    sum = (sum & 0xFFFF) + (sum >> 16);
    sum = (sum & 0xFFFF) + (sum >> 16);
    write_u16_be(ip, 10, !(sum as u16));

    // UDP: swap ports, fix the datagram length, zero the checksum (optional
    // for IPv4, see RFC 768).
    if data + udp_offset + UDP_HDR_LEN > end {
        return Err(());
    }
    let udp = (data + udp_offset) as *mut u8;
    swap_u8(udp, 0, 2);
    swap_u8(udp, 1, 3);
    write_u16_be(udp, 4, (UDP_HDR_LEN + new_dns_len) as u16);
    write_u16_be(udp, 6, 0);

    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: safe byte-level reads/writes for DNS header fields
// ---------------------------------------------------------------------------

/// Copy [`COPY_CHUNK`] bytes, all at constant offsets from `dst`.
#[inline(always)]
fn copy_chunk(dst: usize, src: *const u8, src_off: usize) {
    unsafe {
        *(dst as *mut u8) = *src.add(src_off);
        *((dst + 1) as *mut u8) = *src.add(src_off + 1);
        *((dst + 2) as *mut u8) = *src.add(src_off + 2);
        *((dst + 3) as *mut u8) = *src.add(src_off + 3);
    }
}

#[inline(always)]
fn swap_u8(p: *mut u8, a: usize, b: usize) {
    unsafe {
        let tmp = *p.add(a);
        *p.add(a) = *p.add(b);
        *p.add(b) = tmp;
    }
}

#[inline]
fn read_u16_be(ptr: *const u8, offset: usize) -> u16 {
    unsafe {
        let hi = *ptr.add(offset) as u16;
        let lo = *ptr.add(offset + 1) as u16;
        (hi << 8) | lo
    }
}

#[inline]
fn write_u16_be(ptr: *mut u8, offset: usize, val: u16) {
    unsafe {
        *ptr.add(offset) = (val >> 8) as u8;
        *ptr.add(offset + 1) = (val & 0xFF) as u8;
    }
}

// ---------------------------------------------------------------------------
// Panic handler
// ---------------------------------------------------------------------------

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
