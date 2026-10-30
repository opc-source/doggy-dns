# XDP Kernel Hot Path

Numbers for the kernel-side round on branch `perf/xdp-hot-path`, measured on
2026-10-30 against `origin/main` (`ad6e0d9`).

What changed, in one line each:

- The cache key is hashed **one little-endian word per eight bytes** instead of
  one byte at a time, in both the eBPF program and `doggy-dns-xdp`'s key
  builder.
- The QNAME scan reads the packet in **8-byte blocks**, lowercasing with SWAR
  bit tricks and finding the label terminator without a per-byte loop.
- Cached replies are copied in **16-byte chunks** (two `u64` moves per
  iteration) instead of 4-byte chunks.
- MAC, IP and UDP-port swaps use `u32`/`u16` moves instead of byte pairs.

## How these numbers were taken

Two kinds of measurement, both reproducible from the committed tree:

- **Static instruction counts** come from the compiled object, not from a
  profiler: the `xdp` program section holds one 8-byte slot per eBPF
  instruction, so `readelf` reports the count directly.
- **Component benchmarks** are criterion medians from
  `cargo bench -p doggy-dns-xdp`.

Machine: CherishCai, Apple M1, 8 cores, macOS 26.6.2; the eBPF object was
built inside `localhost/xdp-gcc:snap` (aarch64 container, kernel
7.2.7-200.fc44.aarch64) with the pinned nightly toolchain from
`xdp/rust-toolchain.toml`.

## Static size

| Measurement | Before (`ad6e0d9`) | After | Change |
|-------------|--------------------|-------|--------|
| `xdp` section bytes | 44,576 (`0xae20`) | 46,688 (`0xb660`) | +2,112 |
| eBPF instructions | 5,572 | 5,836 | +264 (+4.7%) |

The program grew because it gained code it never had: the byte-at-a-time tail
for frames with fewer than eight bytes left, the SWAR constants and masks, and
a post-block length check on the terminator path. The hot loops themselves do
less work per byte (below); the total is the sum of a lighter hot path and a
longer edge path.

## Work per unit, from the source

These are properties of the committed code rather than disassembly snippets, so
they are checkable by reading `main.rs` and `protocol.rs`:

| Work | Before | After |
|------|--------|-------|
| QNAME bytes per packet load | 1 | 8 (one `ldxdw`) |
| QNAME bytes per multiply | 1 | 8 |
| QNAME bytes per running-hash spill | 1 | 8 |
| Lowercasing | branchy select per byte | four add/sub/and/or rounds per 8-byte block |
| Memory ops per 16 copied bytes | 32 (16 loads + 16 stores) | 4 (2 `ldxdw` + 2 `stxdw`) |
| Header-swap memory ops | 48 (12 byte-pair exchanges) | 16 (three `u32` and one `u16` swap) |

The hash step matters most for cache hits on short names: the old loop
multiplied once per byte and reloaded the hash from the stack between
multiplies, so an eight-byte QNAME block cost eight serial multiplies and eight
byte loads. The new loop needs one multiply and one load for the same block.

## Component benchmarks (median per iteration)

| Benchmark | Before | After | Change |
|-----------|--------|-------|--------|
| `fnv1a_64_short` (30-byte name) | 13.690 ns | 6.002 ns | −56% |
| `fnv1a_64_long` (255-byte name) | 265.92 ns | 18.653 ns | −93% (14.3×) |
| `make_cache_key` (30-byte name) | 23.784 ns | 23.264 ns | −2% |
| `cache_value_positive` | 47.191 ns | 47.132 ns | unchanged (noise) |
| `cache_value_negative` | 45.170 ns | 45.535 ns | unchanged (noise) |

The gap widens with name length exactly as the multiply count predicts: 30
bytes are four words, 255 bytes are 32 words, and the old loop paid a serial
multiply plus a stack round-trip for each byte of both.

`make_cache_key` barely moves even though the hash under it got faster: the old
build hashed straight from the label bytes, while the new one first assembles
the wire form into a 257-byte stack buffer (uninitialised, so it does not pay a
257-byte memset) and then hashes it. The two costs trade off; what disappears
is the per-byte serial multiply chain, and what appears is a straight buffer
fill the compiler can vectorise. The kernel side gains the full speedup,
because it hashes the packet in place.

## What was not measured

There is no per-packet runtime number for the XDP program itself, and these
are the reasons, so a future round does not repeat the dead ends:

- `BPF_PROG_TEST_RUN` reports a `duration` field, but on this kernel it does
  not reflect the `repeat` loop: 200,000 repetitions are reported as 9 ns in
  total. The field cannot be used as a per-packet timer.
- The same interface stops the repeat loop when the program returns
  `XDP_TX`, so a cache-hit path is counted once per call no matter what
  `repeat` says (verified through the program's own `XDP_STATS` counters:
  `hit` moved by one per call, `miss` by `repeat` per call).
- XDP live-frame mode (`BPF_F_TEST_XDP_LIVE_FRAMES`) runs a different number of
  packets than `repeat × batch` on this kernel and reports `XDP_ABORTED` for a
  program that returns `XDP_TX`, so its timings are not comparable either.
- `bpftool prog profile` refuses aya-built objects (`prog FD 3 doesn't have
  valid btf`).

What backs the change instead: the static counts above, the component
benchmarks, and the seven `xdp_integration_test` cases, which load and attach
the real program on loopback and assert the bytes the client receives:

- `xdp_cache_hit_positive` — the cached reply comes back byte for byte with
  only the transaction id patched.
- `xdp_cache_hit_negative_nxdomain` — the synthesised NXDOMAIN is the query's
  own header with QR=1, RCODE=3, empty other sections, and the question echoed.
- `xdp_cached_reply_edge_lengths` — a root query (a 17-byte DNS message, so the
  byte-at-a-time QNAME fallback runs), a 17-byte cached reply
  (`MIN_POSITIVE_LEN`), and a 32-byte one (a whole number of copy chunks).
- `xdp_cache_ttl_expiry` — an expired entry is passed on, counted, and never
  served stale.
- `xdp_non_dns_passthrough` — a missed query reaches a userspace socket
  unchanged and is counted once; a packet to another port is untouched.
- `xdp_stats_reflect_cache_outcomes` — hit, negative hit, expired and miss
  counters all move, which only happens if the userspace key builder and the
  kernel hash agree, and the map uses the configured capacity.
- `xdp_kernel_logs_reach_userspace` — a poisoned entry's warning is forwarded
  through the ring buffer and drain task.

## Reproduce

```bash
# eBPF object (nightly, bpf-linker)
cd xdp && cargo build -Z build-std=core,alloc --release --target bpfel-unknown-none

# instruction count: one slot per 8 bytes of the xdp section
readelf -SW target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf | grep ' xdp '

# component benchmarks
cargo bench -p doggy-dns-xdp --bench xdp_component_bench

# behaviour (Linux, root, bpffs mounted)
sudo env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored
```
