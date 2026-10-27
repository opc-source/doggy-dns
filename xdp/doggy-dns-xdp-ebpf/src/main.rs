#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::{bpf_ktime_get_ns, bpf_xdp_adjust_tail},
    macros::{map, xdp},
    maps::LruHashMap,
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
const DNS_PORT: u16 = 53;
const MAX_LABELS: usize = 128;
const ETH_HDR_LEN: usize = core::mem::size_of::<EthHdr>();
const UDP_HDR_LEN: usize = core::mem::size_of::<UdpHdr>();
const DNS_HDR_LEN: usize = 12;

// ---------------------------------------------------------------------------
// BPF map
// ---------------------------------------------------------------------------

#[map]
static DNS_CACHE: LruHashMap<CacheKey, CacheValue> =
    LruHashMap::with_max_entries(65536, 0);

// ---------------------------------------------------------------------------
// XDP entry point
// ---------------------------------------------------------------------------

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    match try_dns_xdp(&ctx) {
        Ok(action) => action,
        Err(_) => xdp_action::XDP_PASS,
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
    if u16::from_be(udp.dest) != DNS_PORT {
        return Ok(xdp_action::XDP_PASS);
    }

    // 4. Parse DNS header
    let dns_offset = udp_offset + UDP_HDR_LEN;
    if data + dns_offset + DNS_HDR_LEN > end {
        return Ok(xdp_action::XDP_PASS);
    }
    let dns_ptr = unsafe { (data + dns_offset) as *const u8 };

    // Check QR bit (must be 0 = query)
    let flags = read_u16_be(dns_ptr, 2);
    if flags & 0x8000 != 0 {
        return Ok(xdp_action::XDP_PASS);
    }

    // Check QDCOUNT == 1
    let qdcount = read_u16_be(dns_ptr, 4);
    if qdcount != 1 {
        return Ok(xdp_action::XDP_PASS);
    }

    // 5. Extract QNAME and compute hash
    let (name_hash, qtype, qclass, question_len) =
        match parse_dns_question(ctx, dns_offset) {
            Some(v) => v,
            None => return Ok(xdp_action::XDP_PASS),
        };

    let key = CacheKey {
        name_hash,
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
    };

    // 6. Cache lookup
    let entry = unsafe { DNS_CACHE.get(&key) }
        .ok_or(())?;

    // 7. TTL check
    let now = unsafe { bpf_ktime_get_ns() };
    if entry.expires_at <= now {
        return Ok(xdp_action::XDP_PASS);
    }

    // Save the original TXID (first 2 bytes of DNS header) before any modifications
    let txid = read_u16_be(dns_ptr, 0);

    // 8. Construct response
    if entry.flags & FLAG_NEGATIVE != 0 {
        build_nxdomain_response(ctx, dns_offset, question_len)?;
    } else {
        build_positive_response(ctx, dns_offset, entry, txid)?;
    }

    // 9. Swap headers for reply
    swap_ethernet_headers(ctx);
    swap_ip_headers(ctx, ip_offset);
    swap_udp_headers(ctx, udp_offset);
    recalculate_checksums(ctx, ip_offset, udp_offset);

    Ok(xdp_action::XDP_TX)
}

// ---------------------------------------------------------------------------
// DNS question parsing + FNV-1a hash
// ---------------------------------------------------------------------------

#[inline]
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < data.len() {
        hash ^= data[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

fn parse_dns_question(
    ctx: &XdpContext,
    dns_offset: usize,
) -> Option<(u64, u16, u16, usize)> {
    let data = ctx.data();
    let end = ctx.data_end();
    let mut name_buf = [0u8; 256];
    let mut name_len = 0usize;
    let mut pos = dns_offset + DNS_HDR_LEN;
    let mut labels = 0usize;

    // Parse QNAME label by label (bounded loop for BPF verifier)
    loop {
        if labels >= MAX_LABELS {
            return None;
        }
        // Compare packet-relative addresses, not bare offsets: the verifier
        // only tracks bounds for pointers derived from the packet.
        let cur = data + pos;
        if cur >= end {
            return None;
        }
        let label_len = unsafe { *(cur as *const u8) } as usize;
        pos += 1;

        if label_len == 0 {
            break; // Root label — end of QNAME
        }
        if label_len > 63 || name_len + label_len + 1 > 255 {
            return None;
        }

        // Copy label bytes to buffer (lowercased for case-insensitive matching)
        let mut i = 0;
        while i < label_len {
            let cur = data + pos + i;
            if cur >= end {
                return None;
            }
            let byte = unsafe { *(cur as *const u8) };
            if name_len >= 255 {
                return None;
            }
            name_buf[name_len] = byte.to_ascii_lowercase();
            name_len += 1;
            i += 1;
        }
        if name_len >= 255 {
            return None;
        }
        name_buf[name_len] = b'.';
        name_len += 1;
        pos += label_len;
        labels += 1;
    }

    // After the loop, pos points to QTYPE (the 0x00 root label was consumed
    // inside the loop: we read it, then incremented pos past it).
    // Read QTYPE and QCLASS byte-by-byte to avoid misaligned reads.
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

    // question_len = total length of question section (QNAME + QTYPE + QCLASS)
    let question_total_len = (pos + 4) - dns_offset;

    let name_hash = fnv1a_64(&name_buf[..name_len]);
    Some((name_hash, qtype, qclass, question_total_len))
}

// ---------------------------------------------------------------------------
// Response construction
// ---------------------------------------------------------------------------

fn build_positive_response(
    ctx: &XdpContext,
    dns_offset: usize,
    entry: &CacheValue,
    txid: u16,
) -> Result<(), ()> {
    let resp_len = entry.resp_len as usize;
    if resp_len == 0 || resp_len > 512 {
        return Err(());
    }

    let new_packet_len = dns_offset + resp_len;
    if new_packet_len > 1500 {
        return Err(());
    }

    let current_len = unsafe { ctx.data_end() - ctx.data() };
    let adjust = new_packet_len as i32 - current_len as i32;
    let ret = unsafe { bpf_xdp_adjust_tail(ctx.ctx, adjust) };
    if ret != 0 {
        return Err(());
    }

    // Re-read data/data_end after tail adjustment
    let data = ctx.data();
    let dst = unsafe { (data + dns_offset) as *mut u8 };

    // Copy cached response data into DNS payload
    let src = &entry.resp_data;
    let mut i = 0;
    while i < resp_len {
        unsafe { *dst.add(i) = src[i] };
        i += 1;
    }

    // Preserve the original query's TXID (bytes 0-1 of DNS header)
    // The cached response has its own TXID; overwrite with the query's.
    unsafe {
        *dst.add(0) = (txid >> 8) as u8;
        *dst.add(1) = (txid & 0xFF) as u8;
    }

    Ok(())
}

fn build_nxdomain_response(
    ctx: &XdpContext,
    dns_offset: usize,
    question_len: usize,
) -> Result<(), ()> {
    // Truncate packet to just DNS header + question section
    let new_dns_len = DNS_HDR_LEN + question_len;
    let new_packet_len = dns_offset + new_dns_len;
    let current_len = unsafe { ctx.data_end() - ctx.data() };

    let adjust = new_packet_len as i32 - current_len as i32;
    let ret = unsafe { bpf_xdp_adjust_tail(ctx.ctx, adjust) };
    if ret != 0 {
        return Err(());
    }

    // Re-read data after tail adjustment
    let data = ctx.data();
    let dns_ptr = unsafe { (data + dns_offset) as *mut u8 };

    // Set QR=1, OPCODE=0, AA=0, TC=0, RD=1 (preserve), RA=0, Z=0, RCODE=3 (NXDomain)
    // Flags word at offset 2: 0x8003 = QR=1, RCODE=3
    // Preserve the RD (Recursion Desired) bit from the original query.
    let orig_flags = read_u16_be(dns_ptr as *const u8, 2);
    let rd_bit = orig_flags & 0x0100; // RD is bit 8
    let new_flags = 0x8000 | rd_bit | 0x0003; // QR=1, RCODE=3
    write_u16_be(dns_ptr, 2, new_flags);

    // Set ANCOUNT=0, NSCOUNT=0, ARCOUNT=0
    write_u16_be(dns_ptr, 6, 0);
    write_u16_be(dns_ptr, 8, 0);
    write_u16_be(dns_ptr, 10, 0);

    Ok(())
}

// ---------------------------------------------------------------------------
// Header swap + checksum recalculation
// ---------------------------------------------------------------------------

fn swap_ethernet_headers(ctx: &XdpContext) {
    let data = ctx.data();
    let eth = unsafe { &mut *(data as *mut EthHdr) };
    let tmp = eth.src;
    eth.src = eth.dst;
    eth.dst = tmp;
}

fn swap_ip_headers(ctx: &XdpContext, ip_offset: usize) {
    let data = ctx.data();
    let ip = unsafe { &mut *((data + ip_offset) as *mut IpHdr) };
    let tmp = ip.saddr;
    ip.saddr = ip.daddr;
    ip.daddr = tmp;
    // Set DF flag (Don't Fragment)
    ip.frag_off = (ip.frag_off & 0x00FF) | 0x4000;
}

fn swap_udp_headers(ctx: &XdpContext, udp_offset: usize) {
    let data = ctx.data();
    let udp = unsafe { &mut *((data + udp_offset) as *mut UdpHdr) };
    let tmp = udp.source;
    udp.source = udp.dest;
    udp.dest = tmp;
}

fn recalculate_checksums(ctx: &XdpContext, ip_offset: usize, udp_offset: usize) {
    // Recalculate IPv4 header checksum
    let data = ctx.data();
    let ip = unsafe { &mut *((data + ip_offset) as *mut IpHdr) };
    ip.check = 0;
    let ip_hdr_len = (ip.ihl() as usize) * 4;

    // Simple Internet checksum (RFC 1071) over the IP header
    let mut sum: u32 = 0;
    let ip_bytes = unsafe { core::slice::from_raw_parts(ip as *const IpHdr as *const u8, ip_hdr_len) };
    let mut i = 0;
    while i + 1 < ip_hdr_len {
        sum += ((ip_bytes[i] as u32) << 8) | (ip_bytes[i + 1] as u32);
        i += 2;
    }
    // Fold 32-bit sum into 16 bits
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    ip.check = !(sum as u16);

    // Zero UDP checksum (optional for IPv4/UDP per RFC 768)
    let udp = unsafe { &mut *((data + udp_offset) as *mut UdpHdr) };
    udp.check = 0;
}

// ---------------------------------------------------------------------------
// Helper: safe byte-level reads/writes for DNS header fields
// ---------------------------------------------------------------------------

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
