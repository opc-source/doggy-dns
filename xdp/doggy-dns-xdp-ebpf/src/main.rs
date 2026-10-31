#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::{bpf_ktime_get_ns, bpf_xdp_adjust_tail},
    macros::{map, xdp},
    maps::{Array, LruHashMap, PerCpuArray},
    programs::XdpContext,
};
use aya_log_ebpf::{error, warn};

mod protocol;
mod types;

use protocol::{
    COPY_CHUNK, ETH_HDR_LEN, ETH_P_IP, EthHdr, IPPROTO_UDP, IpHdr, UDP_HDR_LEN, UdpHdr, copy_chunk,
    read_u16_be, read_u16_be_volatile, swap_u16, swap_u32, write_u16_be,
};
use types::{CacheKey, CacheValue, FLAG_NEGATIVE, XdpRuntimeConfig, XdpStats};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// A QNAME is at most 255 bytes on the wire, including the root label.
const MAX_QNAME_LEN: usize = 255;
const DNS_HDR_LEN: usize = 12;
const MAX_DNS_LEN: usize = 512;
// Largest UDP/L4 length the fast path will touch. Only queries that carry a
// single question ever match the cache, so anything above this is passed on.
const MAX_UDP_DNS_LEN: usize = 1500;
// The shortest possible positive reply: header, a one-byte QNAME (the root)
// and QTYPE/QCLASS. The 16-byte copy chunk relies on this, because it always
// writes a whole chunk even when the reply length is not a multiple of it.
const MIN_POSITIVE_LEN: usize = DNS_HDR_LEN + 1 + 4;

/// Why a packet fetched or matched nothing in the cache.
enum Outcome {
    Hit,
    NegativeHit,
    Miss,
    Expired,
    Drop,
}

// ---------------------------------------------------------------------------
// BPF maps
// ---------------------------------------------------------------------------

#[map]
static DNS_CACHE: LruHashMap<CacheKey, CacheValue> = LruHashMap::with_max_entries(65536, 0);

/// Single-entry map holding the settings the userspace loader supplies.
#[map]
static XDP_CONFIG: Array<XdpRuntimeConfig> = Array::with_max_entries(1, 0);

/// Per-CPU counters classified at the single exit point of the program.
#[map]
static XDP_STATS: PerCpuArray<XdpStats> = PerCpuArray::with_max_entries(1, 0);

// ---------------------------------------------------------------------------
// XDP entry point
// ---------------------------------------------------------------------------

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    let (action, outcome) = match try_dns_xdp(&ctx) {
        Ok(result) => result,
        // Errors only happen after the frame has been resized, so the packet
        // can no longer be handed on as the original query. Every failure site
        // logs before returning `Err`, so this catch-all stays silent.
        Err(()) => (xdp_action::XDP_DROP, Some(Outcome::Drop)),
    };
    if let Some(outcome) = outcome {
        record(outcome);
    }
    action
}

/// Increment the current CPU's counter. One map lookup per classified packet;
/// non-DNS traffic never reaches this path.
#[inline(always)]
fn record(outcome: Outcome) {
    let Some(stats) = XDP_STATS.get_ptr_mut(0) else {
        return;
    };
    // Safety: the per-CPU map value belongs to the current CPU, and XDP runs
    // with bottom halves disabled, so no other instance can write this slot.
    unsafe {
        match outcome {
            Outcome::Hit => (*stats).hit += 1,
            Outcome::NegativeHit => (*stats).negative_hit += 1,
            Outcome::Miss => (*stats).miss += 1,
            Outcome::Expired => (*stats).expired += 1,
            Outcome::Drop => (*stats).drop += 1,
        }
    }
}

fn try_dns_xdp(ctx: &XdpContext) -> Result<(u32, Option<Outcome>), ()> {
    let data = ctx.data();
    let end = ctx.data_end();

    // 1. Parse Ethernet header
    if data + ETH_HDR_LEN > end {
        return Ok((xdp_action::XDP_PASS, None));
    }
    let eth = unsafe { &*(data as *const EthHdr) };
    if u16::from_be(eth.h_proto) != ETH_P_IP {
        return Ok((xdp_action::XDP_PASS, None));
    }

    // 2. Parse IPv4 header
    let ip_offset = ETH_HDR_LEN;
    if data + ip_offset + core::mem::size_of::<IpHdr>() > end {
        return Ok((xdp_action::XDP_PASS, None));
    }
    let ip = unsafe { &*((data + ip_offset) as *const IpHdr) };
    if ip.protocol != IPPROTO_UDP {
        return Ok((xdp_action::XDP_PASS, None));
    }
    let ip_hdr_len = (ip.ihl() as usize) * 4;
    if ip_hdr_len < 20 || data + ip_offset + ip_hdr_len > end {
        return Ok((xdp_action::XDP_PASS, None));
    }

    // 3. Parse UDP header
    let udp_offset = ip_offset + ip_hdr_len;
    if data + udp_offset + UDP_HDR_LEN > end {
        return Ok((xdp_action::XDP_PASS, None));
    }
    let udp = unsafe { &*((data + udp_offset) as *const UdpHdr) };
    // The userspace server may listen on any port, so it is read from the
    // configuration map the loader fills in before attaching. A missing entry
    // means the program was loaded without its runtime configuration, so the
    // fast path silently degrades to pass-through — warn on every such packet.
    let Some(config) = XDP_CONFIG.get(0) else {
        warn!(ctx, "XDP_CONFIG entry 0 missing, passing packet");
        return Ok((xdp_action::XDP_PASS, None));
    };
    if u16::from_be(udp.dest) != config.dns_port {
        return Ok((xdp_action::XDP_PASS, None));
    }
    // Volatile byte loads plus an additive compose stop LLVM from folding
    // this into `be16`; both that and a register `|` lose the 16-bit range in
    // this kernel's verifier, and the length is later added to a packet
    // pointer. It must stay bounded even after the compiler spills it.
    let udp_len = read_u16_be_volatile((data + udp_offset) as *const u8, 4) as usize;
    if !(UDP_HDR_LEN..=MAX_UDP_DNS_LEN).contains(&udp_len) {
        return Ok((xdp_action::XDP_PASS, None));
    }
    // Current DNS message length, taken from the UDP length field. The packet
    // length cannot be derived from `data_end - data`: the verifier rejects any
    // arithmetic involving a PTR_TO_PACKET_END register.
    let old_dns_len = udp_len - UDP_HDR_LEN;

    // 4. Parse DNS header
    let dns_offset = udp_offset + UDP_HDR_LEN;
    if data + dns_offset + DNS_HDR_LEN > end {
        return Ok((xdp_action::XDP_PASS, None));
    }
    let dns_ptr = (data + dns_offset) as *const u8;

    // Query must have QR=0 and QDCOUNT=1
    let flags = read_u16_be(dns_ptr, 2);
    if flags & 0x8000 != 0 || read_u16_be(dns_ptr, 4) != 1 {
        return Ok((xdp_action::XDP_PASS, None));
    }

    // 5. Extract QNAME and compute hash
    let (name_hash, qtype, qclass, question_len) = match parse_dns_question(ctx, dns_offset) {
        Some(v) => v,
        None => return Ok((xdp_action::XDP_PASS, None)),
    };

    let key = CacheKey {
        name_hash,
        qtype_class: ((qtype as u32) << 16) | (qclass as u32),
        _pad: 0,
    };

    // 6. Cache lookup
    let entry = match unsafe { DNS_CACHE.get(&key) } {
        Some(entry) => entry,
        None => return Ok((xdp_action::XDP_PASS, Some(Outcome::Miss))),
    };

    // 7. TTL check
    let now = unsafe { bpf_ktime_get_ns() };
    if entry.expires_at <= now {
        return Ok((xdp_action::XDP_PASS, Some(Outcome::Expired)));
    }

    let txid = read_u16_be(dns_ptr, 0);

    // The query must be fully contained in the frame before we resize it.
    if data + dns_offset + old_dns_len > end {
        warn!(
            ctx,
            "truncated query while occupying the frame, passing (txid {:x}, hash {:x}, qtype {})",
            txid as u64,
            name_hash,
            qtype as u64
        );
        return Ok((xdp_action::XDP_PASS, None));
    }

    let negative = entry.flags & FLAG_NEGATIVE != 0;
    let resp_len = entry.resp_len as usize;
    let new_dns_len = if negative {
        DNS_HDR_LEN + question_len
    } else {
        // The duplicate bound inside `write_positive` exists for the verifier
        // and stays silent; this is the diagnostic site for a poisoned entry.
        if resp_len < MIN_POSITIVE_LEN || resp_len > MAX_DNS_LEN {
            warn!(
                ctx,
                "cached response length out of range: {} (txid {:x}, hash {:x})",
                resp_len as u64,
                txid as u64,
                name_hash
            );
            return Ok((xdp_action::XDP_PASS, None));
        }
        resp_len
    };

    // 8. Resize the frame to the new DNS message length.
    let delta = new_dns_len as i32 - old_dns_len as i32;
    if unsafe { bpf_xdp_adjust_tail(ctx.ctx, delta) } != 0 {
        warn!(
            ctx,
            "bpf_xdp_adjust_tail failed (old {}, new {}, txid {:x}, hash {:x}), passing packet",
            old_dns_len as u64,
            new_dns_len as u64,
            txid as u64,
            name_hash,
        );
        return Ok((xdp_action::XDP_PASS, None));
    }

    // Past this point the frame has been resized, so every failure returns
    // `Err` and the packet is dropped instead of being passed on: the original
    // query no longer exists.
    let data = ctx.data();
    let end = ctx.data_end();
    if data + dns_offset + new_dns_len > end {
        error!(
            ctx,
            "frame too small after resize (need {} bytes at offset {}, txid {:x}, hash {:x}, qtype {})",
            new_dns_len as u64,
            dns_offset as u64,
            txid as u64,
            name_hash,
            qtype as u64
        );
        return Err(());
    }
    let dns = data + dns_offset;

    if negative {
        write_nxdomain(ctx, dns, end, txid)?;
    } else {
        write_positive(ctx, dns, end, entry, resp_len, txid)?;
    }

    // 9. Swap headers for reply, fix lengths, and recompute checksums.
    finalize_headers(ctx, ip_hdr_len, udp_offset, new_dns_len, txid)?;

    let outcome = if negative {
        Outcome::NegativeHit
    } else {
        Outcome::Hit
    };

    Ok((xdp_action::XDP_TX, Some(outcome)))
}

// ---------------------------------------------------------------------------
// DNS question parsing + word-wise FNV-1a hash
// ---------------------------------------------------------------------------

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

const BYTE_ONES: u64 = 0x0101_0101_0101_0101;
const BYTE_HIGHS: u64 = 0x8080_8080_8080_8080;

/// Fold one little-endian 8-byte word of QNAME into the running hash.
#[inline(always)]
fn fnv1a_word(hash: u64, word: u64) -> u64 {
    (hash ^ word).wrapping_mul(FNV_PRIME)
}

/// Lowercase ASCII letters in every byte, branchlessly.
///
/// `b >= 0x41` and `b >= 0x5B` come from the padded-subtraction SWAR trick:
/// `word | BYTE_HIGHS` keeps each byte's borrow to itself, and `!word` drops
/// bytes that already have the high bit set (never ASCII letters).
#[inline(always)]
fn lower_word(word: u64) -> u64 {
    let ge_a = ((word | BYTE_HIGHS) - 0x4141_4141_4141_4141) & !word & BYTE_HIGHS;
    let gt_z = ((word | BYTE_HIGHS) - 0x5B5B_5B5B_5B5B_5B5B) & !word & BYTE_HIGHS;
    // `'A'..='Z'` leaves bit 7 set; moving it to bit 5 gives the +0x20 offset.
    word | ((ge_a & !gt_z) >> 2)
}

/// Index of the first zero byte in `word`, or 8 if there is none.
///
/// Only called for the block that holds the root label, so the eight-way
/// scan runs once per packet rather than once per word.
#[inline(always)]
fn zero_index(word: u64) -> usize {
    let mut i = 0;
    while i < 8 {
        if ((word >> (i * 8)) & 0xFF) == 0 {
            return i;
        }
        i += 1;
    }
    8
}

fn parse_dns_question(ctx: &XdpContext, dns_offset: usize) -> Option<(u64, u16, u16, usize)> {
    let data = ctx.data();
    let end = ctx.data_end();
    // The QNAME is hashed as its wire bytes (length-prefixed labels,
    // lowercased, root label's zero included) zero-padded to a whole 8-byte
    // word and folded in one little-endian u64 at a time — the same shape as
    // `fnv1a_64` in the userspace key builder. Besides running one multiply
    // per eight bytes instead of one per byte, a word read needs a single
    // bounds check instead of one per byte.
    let mut hash = FNV_OFFSET_BASIS;
    let mut pos = dns_offset + DNS_HDR_LEN;
    let mut name_len = 0usize;

    loop {
        if name_len >= MAX_QNAME_LEN {
            return None;
        }
        let cur = data + pos;
        if cur + 8 <= end {
            // Safety: the comparison above proves eight readable bytes.
            // A plain deref, not `read_unaligned`: LLVM spills the latter
            // into eight byte loads, while the verifier accepts unaligned
            // packet accesses and emits one `ldxdw`.
            let word = unsafe { *(cur as *const u64) };
            let zeros = word.wrapping_sub(BYTE_ONES) & !word & BYTE_HIGHS;
            if zeros == 0 {
                hash = fnv1a_word(hash, lower_word(word));
                pos += 8;
                name_len += 8;
                continue;
            }
            // This block holds the root label: keep the bytes up to the
            // terminator and drop everything after it, which is the zero
            // padding the userspace side hashes.
            let consumed = zero_index(word) + 1;
            // The terminator can sit up to seven bytes past the top-of-loop
            // check, so the block path is the one place a name past the wire
            // limit would slip through: bound the post-block length here.
            if name_len + consumed > MAX_QNAME_LEN {
                return None;
            }
            let first = zeros & zeros.wrapping_neg();
            hash = fnv1a_word(hash, lower_word(word) & (first - 1));
            pos += consumed;
            name_len += consumed;
            break;
        }
        // Fewer than eight bytes remain in the frame: assemble the same
        // little-endian word one byte at a time.
        let mut word = 0u64;
        let mut i = 0usize;
        let mut terminated = false;
        while i < 8 {
            if name_len >= MAX_QNAME_LEN {
                return None;
            }
            let p = data + pos;
            if p + 2 > end {
                return None;
            }
            let byte = unsafe { *(p as *const u8) };
            word |= (byte.to_ascii_lowercase() as u64) << (i * 8);
            pos += 1;
            name_len += 1;
            i += 1;
            if byte == 0 {
                terminated = true;
                break;
            }
        }
        if !terminated {
            return None;
        }
        hash = fnv1a_word(hash, word);
        break;
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
///
/// Failures happen after the frame was resized, so the packet is dropped and
/// each site logs at error level before returning.
#[inline(always)]
fn write_nxdomain(ctx: &XdpContext, dns: usize, end: usize, txid: u16) -> Result<(), ()> {
    // A constant offset keeps the packet register's id, so the range proven by
    // this comparison also covers every access below.
    if dns + DNS_HDR_LEN > end {
        error!(
            ctx,
            "nxdomain header does not fit after resize ({} bytes, txid {:x})",
            DNS_HDR_LEN as u64,
            txid as u64
        );
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
    ctx: &XdpContext,
    dns: usize,
    end: usize,
    entry: &CacheValue,
    resp_len: usize,
    txid: u16,
) -> Result<(), ()> {
    // Verifier bound only: the poisoned-entry case is diagnosed and passed
    // before the resize, and reaching this point means the frame was resized
    // and must be answered or dropped.
    if resp_len < MIN_POSITIVE_LEN || resp_len > MAX_DNS_LEN {
        return Err(());
    }

    let src = entry.resp_data.as_ptr();
    // Copy the leading chunk so that the loop below is aligned to it and ends
    // exactly at resp_len. resp_len >= MIN_POSITIVE_LEN, so a full chunk is
    // always available even when resp_len is not a multiple of it.
    let lead = resp_len & (COPY_CHUNK - 1);
    let mut i = lead;
    if lead != 0 {
        if dns + COPY_CHUNK > end {
            error!(
                ctx,
                "truncated frame while copying cached response (txid {:x})", txid as u64
            );
            return Err(());
        }
        copy_chunk(dns, src, 0);
    }
    while i < resp_len {
        let p = dns + i;
        if p + COPY_CHUNK > end {
            error!(
                ctx,
                "truncated frame while copying cached response (txid {:x})", txid as u64
            );
            return Err(());
        }
        copy_chunk(p, src, i);
        i += COPY_CHUNK;
    }

    // The cached reply carries the transaction id of the query that populated
    // the cache; overwrite it with the current query's id.
    if dns + 2 > end {
        error!(
            ctx,
            "truncated frame while writing transaction id (txid {:x})", txid as u64
        );
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
    txid: u16,
) -> Result<(), ()> {
    let data = ctx.data();
    let end = ctx.data_end();

    // Ethernet: swap source and destination MAC addresses. Every access below
    // has a constant offset, so this single check covers all of them.
    if data + ETH_HDR_LEN > end {
        error!(
            ctx,
            "ethernet header missing after resize (txid {:x})", txid as u64
        );
        return Err(());
    }
    let eth = data as *mut u8;
    // Two 4-byte halves of each 6-byte address: dst[0..4] <-> src[0..4] and
    // dst[4..6] <-> src[4..6].
    swap_u32(eth, 0, 6);
    swap_u16(eth, 4, 10);

    // IPv4: swap addresses, set DF, fix the total length and update the header
    // checksum. The checksum is updated incrementally (RFC 1624) rather than
    // recomputed over the whole header: a header walk needs a variable offset,
    // and the verifier only accepts variable offsets that are re-checked
    // against data_end on every iteration.
    if data + ETH_HDR_LEN + core::mem::size_of::<IpHdr>() > end {
        error!(
            ctx,
            "ipv4 header missing after resize (txid {:x})", txid as u64
        );
        return Err(());
    }
    let ip = (data + ETH_HDR_LEN) as *mut u8;
    let old_tot_len = read_u16_be(ip, 2);
    let old_frag_off = read_u16_be(ip, 6);
    let old_check = read_u16_be(ip, 10);

    // Swapping the two addresses leaves the one's complement sum unchanged.
    swap_u32(ip, 12, 16);

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
        error!(
            ctx,
            "udp header missing after resize (txid {:x})", txid as u64
        );
        return Err(());
    }
    let udp = (data + udp_offset) as *mut u8;
    swap_u16(udp, 0, 2);
    write_u16_be(udp, 4, (UDP_HDR_LEN + new_dns_len) as u16);
    write_u16_be(udp, 6, 0);

    Ok(())
}

// ---------------------------------------------------------------------------
// Panic handler
// ---------------------------------------------------------------------------

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
