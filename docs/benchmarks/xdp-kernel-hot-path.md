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
  instruction, so `readelf` reports the count directly, and loop sizes are the
  address spans between a loop's head and its back edge in `objdump -d -m bpf`.
- **Component benchmarks** are criterion medians from
  `cargo bench -p doggy-dns-xdp`.

Machine: CherishCai, Apple M1, 8 cores, macOS 26.6.2; the eBPF object was
built and measured inside `localhost/xdp-gcc:snap` (aarch64 Linux container,
kernel 7.2.7) with the pinned nightly toolchain from `xdp/rust-toolchain.toml`.

## Static size

| Measurement | Before (`ad6e0d9`) | After | Change |
|-------------|--------------------|-------|--------|
| `xdp` section bytes | 44,576 (`0xae20`) | 46,384 (`0xb530`) | +1,808 |
| eBPF instructions | 5,572 | 5,798 | +226 (+4.1%) |

The program grew because the new paths are longer than the loops they
replaced — the byte-at-a-time tail for frames with fewer than eight bytes left,
the SWAR constants themselves, and the cold logging blocks are unchanged but
now sit further from the hot path. The hot loops shrank (below); the total is
the sum of a smaller hot path and a longer cold one.

## Hot loops

Counted from the disassembly (instruction slots, `lddw` counting as its two
slots). Both programs are loaded from the same sources with the same
toolchain.

| Loop | Before | After |
|------|--------|-------|
| QNAME parse + hash | 26 slots per **byte** (≈208 per 8 bytes): one `mul`, one stack reload of the hash, a byte load, a branchy lowercase select and a bounds check per byte | 46 slots per **8 bytes**: one `ldxdw`, one `mul`, SWAR lowercase, zero detection, one hash spill |
| Reply copy | 19 slots per **4 bytes** (≈76 per 16 bytes): bounds re-check plus four byte copy pairs | 15 slots per **16 bytes**: bounds re-check plus two `ldxdw`/`stxdw` pairs |
| MAC/IP/port swap | 12 `swap_u8` calls, 4 slots each (48) | 2 `u32` moves, 1 `u16` move for the MAC; 1 `u32` per IP address; 1 `u16` for the ports (16) |

The hash step is the part that matters most for cache hits on short names: the
old loop multiplied once per byte and reloaded the hash from the stack between
multiplies, so an 8-byte QNAME block cost eight serial multiplies. The new
block loop multiplies once and does the lowercasing for all eight bytes with
four add/sub/and/or rounds.

## Component benchmarks (median per iteration)

| Benchmark | Before | After | Change |
|-----------|--------|-------|--------|
| `fnv1a_64_short` (22-byte name) | 13.913 ns | 5.991 ns | −57% |
| `fnv1a_64_long` (32-byte name) | 14.578 ns | 6.313 ns | −57% |
| `make_cache_key` | 23.784 ns | 23.264 ns | −2% |
| `cache_value_positive` | 47.191 ns | 47.132 ns | unchanged (noise) |
| `cache_value_negative` | 45.170 ns | 45.535 ns | unchanged (noise) |

`make_cache_key` barely moves even though the hash under it got 2.3× faster:
the old build hashed straight from the label bytes, while the new one first
assembles the wire form into a 257-byte stack buffer (uninitialised, so it does
not pay a 257-byte memset) and then hashes it. The two costs trade off; what
disappears is the per-byte serial multiply chain, and what appears is a
straight buffer fill that the compiler can vectorise. The kernel side gains the
full 2.3×, because it hashes the packet in place.

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
benchmarks, and the six `xdp_integration_test` cases — they load and attach the
real program and check cache hits, NXDOMAIN synthesis, TTL expiry, the poisoned
entry path, kernel log delivery and non-DNS pass-through, including a cache
entry built by the userspace key builder being served by the kernel program,
which is what pins the two hash implementations to each other.

## Reproduce

```bash
# eBPF object (nightly, bpf-linker)
cd xdp && cargo build --release --target bpfel-unknown-none

# instruction count: one slot per 8 bytes of the xdp section
readelf -SW target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf | grep ' xdp '
objdump -d -m bpf target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf

# component benchmarks
cargo bench -p doggy-dns-xdp

# behaviour (Linux, root, bpffs mounted)
sudo env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored
```
