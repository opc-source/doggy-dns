# Allocator A/B: system vs jemalloc vs mimalloc

This records the local measurements that decided the opt-in `jemalloc` build
feature and its platform gate. They are local experiments, not a CI baseline;
the `Performance` job now runs a system/jemalloc pair as a report-only matrix.

- Date: 2026-11-22
- Machine: podman `libkrun` VM, aarch64, 4 vCPU, 7.45 GiB
- Container: `docker.io/library/rust:1-slim-bookworm` (Debian 12, glibc 2.36),
  rustc 1.100.0
- Build: `[profile.release]` as committed (`lto = "fat"`, `codegen-units = 1`);
  the default and `--features jemalloc` binaries come from this tree, the
  mimalloc one from a local, uncommitted feature patch that wired the allocator
  the same way (never merged, so mimalloc is not reproducible from the
  repository). The final manifest keeps only `jemalloc`: no target in the image
  matrix needs mimalloc (it exists in agentgateway for arm64-musl).
- Load: 1000 names (`svc-N.perf.doggy.test A`), `forward` authority pointed at
  `scripts/perf_upstream.c` on loopback (TTL 300), `worker_threads = 2`,
  dnsperf 2.10.0 `-T 2 -c 2 -n 100000 -l 20`, one 5 s warm-up pass, fresh
  server per round. `RUST_LOG=warn`.
- Rounds: three in the order system → mimalloc → jemalloc, then three in the
  reverse order, 18 measured runs total. Every round reported
  `Queries lost: 0` and an upstream delta of 0, i.e. the measured traffic was
  answered from the forward resolver's cache and did not include the upstream.
- CPU is the server process's `utime + stime` from `/proc/<pid>/stat` over the
  measured window; RSS is `VmHWM` from `/proc/<pid>/status` at the end.
- Allocator actually installed was checked in the startup log
  (`allocator="system" | "mimalloc" | "jemalloc"`) and in the binaries'
  symbols/strings (`mi_free`, jemalloc version strings).

## Summary

| arm | median QPS (6 rounds) | range | CPU per query | peak RSS |
|---|---:|---:|---:|---:|
| system (glibc 2.36) | 254,157 | 248,845–260,846 | 5.87 µs | 13.9 MiB |
| mimalloc | 285,519 (+12.3%) | 278,295–287,434 | 5.13 µs (−12.6%) | 22.3 MiB |
| jemalloc | 287,419 (+13.1%) | 282,064–291,092 | 5.00 µs (−14.8%) | 16.2 MiB |

The system and allocator ranges do not overlap. mimalloc and jemalloc are
within noise of each other on QPS; jemalloc does slightly less work per query
and costs about 2.3 MiB more RSS instead of mimalloc's 8.4 MiB.

## Per-round data

| sweep | arm | round | QPS | completed | CPU s | peak RSS |
|---|---|---:|---:|---:|---:|---:|
| forward | system | 1 | 249,632 | 4,992,747 | 29.10 | 13.9 MiB |
| forward | mimalloc | 1 | 281,880 | 5,637,690 | 29.07 | 22.4 MiB |
| forward | jemalloc | 1 | 282,064 | 5,641,381 | 28.18 | 16.5 MiB |
| forward | system | 2 | 248,845 | 4,976,986 | 29.65 | 14.2 MiB |
| forward | mimalloc | 2 | 278,295 | 5,565,977 | 28.87 | 22.4 MiB |
| forward | jemalloc | 2 | 286,399 | 5,728,045 | 28.72 | 16.1 MiB |
| forward | system | 3 | 258,767 | 5,175,418 | 29.80 | 13.9 MiB |
| forward | mimalloc | 3 | 285,583 | 5,711,746 | 29.18 | 22.3 MiB |
| forward | jemalloc | 3 | 286,166 | 5,723,424 | 28.79 | 16.2 MiB |
| reverse | jemalloc | 1 | 288,438 | 5,768,859 | 28.98 | 15.9 MiB |
| reverse | mimalloc | 1 | 285,934 | 5,718,790 | 29.37 | 22.4 MiB |
| reverse | system | 1 | 253,334 | 5,066,766 | 30.13 | 14.0 MiB |
| reverse | jemalloc | 2 | 291,092 | 5,822,626 | 28.93 | 16.1 MiB |
| reverse | mimalloc | 2 | 287,434 | 5,748,751 | 29.25 | 22.3 MiB |
| reverse | system | 2 | 254,981 | 5,099,714 | 30.16 | 13.4 MiB |
| reverse | jemalloc | 3 | 288,556 | 5,771,193 | 28.76 | 16.4 MiB |
| reverse | mimalloc | 3 | 285,454 | 5,709,199 | 29.24 | 22.3 MiB |
| reverse | system | 3 | 260,846 | 5,217,016 | 30.28 | 13.6 MiB |

## Reading

- The gain is not extra CPU spent for more answers: server CPU seconds are flat
  across arms (~28–30 s), and completed queries rise with the allocator, so the
  improvement is per-query CPU (5.87 → 5.00 µs for jemalloc, −14.8%).
- The macOS profile in `macos-local-test-report.md` attributes 15.7% (wt=1) /
  22.2% (wt=8) of on-CPU samples to alloc/free/memmove/memset, but only about
  11% at wt=8 are `malloc`/`free` leaves; the rest is memmove/memset on the
  per-request buffer. Removing an 11.2% CPU share entirely caps the speedup at
  `1/(1-0.112) ≈ +12.6%`, close to the measured +13.1%; the leaf share is not
  itself the QPS delta.
- This is the `forward` path with a warm resolver cache, which still clones
  answer records per reply. The Nacos path allocates less (5–6 allocations per
  lookup, pinned by `alloc_guard_test`), so its allocator gain may be smaller
  and is not measured here. Treat the numbers as an upper-bound-ish signal for
  the production mix, not as a Nacos figure.
- The table above is aarch64 glibc. x86_64 glibc is the other image target and
  was not measured; macOS aarch64 is measured in its own section below.
- The measured binary used jemalloc's default `lg_page` (12, for 4 KiB pages).
  The arm64 image build sets `JEMALLOC_SYS_WITH_LG_PAGE=16` so the same image
  runs on 4, 16 and 64 KiB-page hosts; alignment and RSS may differ slightly
  from these numbers.
- The arms ran sequentially, with the reverse sweep as an order-bias control;
  on a shared runner a same-run pair is indicative only, not a gated
  comparison.

## macOS aarch64

The crate's platform table lists `aarch64-apple-darwin` as build ✓ / run ✓. With
the same non-musl, non-MSVC gate, `--features jemalloc` installs jemalloc on
macOS too: a functional run reported `allocator="jemalloc"` and answered a query
through the loopback upstream.

- Machine: Apple M1, 8 cores, macOS 27.2.1, rustc 1.100.0, dnsperf 2.16.0
- Same load as the Linux table: 1000 names, warm-cache forward to a loopback
  upstream, `worker_threads = 2`, `-T2 -c2 -n100000 -l20`, three forward plus
  three reverse rounds per arm, 0 lost, upstream delta 0. The driver is the
  local `perf-results/bench/run_bench.py` (port 15653), not the container flow
  in Reproduce.
- CPU is `ps -o cputime=` before/after the 20 s window (no procfs on macOS);
  µs/query = Σ CPU seconds ÷ Σ completed: system 175.87 s / 13,207,364 =
  13.32 µs, jemalloc 172.85 s / 13,468,388 = 12.83 µs.
- The macOS jemalloc is built at the OS page size (16 KiB) and uses no
  `malloc_conf`; `JEMALLOC_SYS_WITH_LG_PAGE=16` is a Linux-image concern.
- macOS support is locally verified, not built in CI; the CI matrix is Linux.

| arm | median QPS | range | CPU per query | latency |
|---|---:|---:|---:|---:|
| system (macOS xzone) | 111,048 | 106,630–111,584 | 13.32 µs | 0.843 ms |
| jemalloc | 112,396 (+1.2%) | 110,928–112,708 | 12.83 µs | 0.833 ms |

The ranges overlap here (system 106,630–111,584, jemalloc 110,928–112,708),
unlike the Linux table: jemalloc's slowest round is below the system median.
Read this as parity to slightly better on this host, within run-to-run spread,
not as a gated +1.2% win. The gain is far smaller than Linux's ~13% because the
baseline is Apple's xzone malloc, not glibc; the harness and CPU differ too.
CPU per query drops ~3.7% (13.32 → 12.83 µs). Do not carry the Linux numbers
across operating systems; this row only says the opt-in build works and is not
a regression on the dev platform.

## malloc_conf arms

The Linux run above left jemalloc at its defaults. A second run asked whether the
agentgateway tuning pays off here: one jemalloc binary, five arms selected at
runtime. The environment variable this build actually reads is
**`_RJEM_MALLOC_CONF`**, not `MALLOC_CONF`: tikv-jemallocator links jemalloc
with the `_rjem_` symbol prefix, and jemalloc's configure upper-cases that
prefix for the env name. Setting `MALLOC_CONF` is silently ignored — verified
by passing an invalid option with `abort_conf:true`: the server started
normally under `MALLOC_CONF`, and only aborted under `_RJEM_MALLOC_CONF`.

Host: `/sys/kernel/mm/transparent_hugepage/enabled` was
`always [madvise] never` — active mode `madvise` — 4 KiB pages, kernel 7.2.7
aarch64. Same load and round protocol as above (3 forward + 3 reverse).

| arm | added to `_RJEM_MALLOC_CONF` | median QPS | range | CPU per query | peak RSS | threads |
|---|---|---:|---:|---:|---:|---:|
| default | — | 288,771 | 287,401–290,667 | 4.98 µs | 15.8 MiB | 3 |
| bg | `background_thread:true` | 288,961 (+0.07%) | 286,293–290,292 | 5.00 µs | 16.8 MiB | 6 |
| bg_thp | + `thp:never` | 287,607 (−0.40%) | 286,585–289,492 | 4.99 µs | 16.8 MiB | 6 |
| bg_decay | + `dirty_decay_ms:5000,muzzy_decay_ms:5000` | 289,060 (+0.10%) | 284,303–290,901 | 4.99 µs | 17.1 MiB | 6 |
| bg_percpu | + `percpu_arena:percpu` | 288,014 (−0.26%) | 284,967–290,485 | 4.99 µs | 16.6 MiB | 5–6 |

Every arm's median QPS is within ±0.4% of the default and the ranges overlap;
CPU per query is flat at ~5.0 µs. `background_thread:true` does create the threads (3 → 5–6)
and costs about 1 MiB of peak RSS, but buys nothing measurable here. `thp:never`
changed nothing; with the host in `madvise` mode jemalloc would not have used
huge pages by default anyway, so this run does not test an `always` host. The
decay and per-CPU-arena options are equally flat.

The dnsperf 2.10 report has average/min/max only, so the tail-latency rationale
for background purging is not resolved by this harness. Revisit it only if
production shows purge-induced p99 spikes, measured with a percentile-capable
client. `prof:true` was never in scope: it exists for pprof_alloc heap
profiling and costs sampling.

**Decision:** keep jemalloc defaults; no `malloc_conf` is baked into the binary.

If a conf is ever needed, it has to match the prefix: either set
`JEMALLOC_SYS_WITH_MALLOC_CONF` when building (the sys crate turns it into
`--with-malloc-conf`), or enable tikv-jemallocator's
`unprefixed_malloc_on_supported_platforms` feature and export `malloc_conf` as
agentgateway does. A `malloc_conf` symbol in the prefixed build is silently
ignored — jemalloc looks for `_rjem_malloc_conf`.

Raw per-arm data, `confirm_conf` output and the harness are under the local
`perf-results/bench/out/allocator-ab-2026-11-22/malloc-conf/` directory (the
`perf-results` tree is gitignored).

## Reproduce

The harness was a local scratch script, not a committed tool. The essential
container and load commands:

```bash
podman run -d --name doggy-alloc-bench \
  -v "$PWD":/src \
  -v doggy-alloc-cargo:/usr/local/cargo/registry \
  -v doggy-alloc-target:/target \
  -e CARGO_TARGET_DIR=/target -w /src \
  docker.io/library/rust:1-slim-bookworm sleep infinity
podman exec doggy-alloc-bench bash -c \
  'apt-get update && apt-get install -y --no-install-recommends \
     build-essential clang cmake libclang-dev perl pkg-config git dnsperf dnsutils'
podman exec doggy-alloc-bench bash -c \
  'mkdir -p /out && cd /src \
     && cargo build --release --locked \
     && cp /target/release/doggy-dns /out/doggy-dns-system \
     && cargo build --release --locked --features jemalloc \
     && cp /target/release/doggy-dns /out/doggy-dns-jemalloc \
     && cc -O2 -static -o /out/perf-upstream scripts/perf_upstream.c'
```

Server config (the system arm is the same binary built without the feature):

```toml
[server]
listen_addr = "127.0.0.1"
port = 15353
worker_threads = 2

[xdp]
enabled = false

[[plugins]]
kind = "forward"
enabled = true
upstream = ["127.0.0.1:15354"]
```

Each round: start `perf-upstream --port 15354 --count-file …`, start the server,
wait for `dig @127.0.0.1 -p 15353 ready.perf.doggy.test` to return
`127.0.0.9`, run one 5 s warm-up, then
`dnsperf -s 127.0.0.1 -p 15353 -d queries.txt -T 2 -c 2 -n 100000 -l 20`.
Read `/proc/<pid>/stat` before and after, and `VmHWM` before the kill.
`SIGUSR1` to the upstream plus its count file gives the cached-answer guard.

## Using the feature

- `jemalloc` is off by default. The dependency and `src/main.rs` use
  `cfg(all(not(target_env = "msvc"), not(target_env = "musl")))`, so any
  non-musl, non-MSVC target attempts the build; only Linux/glibc and macOS are
  verified here, and on musl or MSVC the feature keeps the system allocator.
  musl is excluded conservatively: it is not among the crate's tested targets,
  background-thread support is disabled there, and no image uses musl. MSVC is upstream's own exclusion — tikv-jemalloc-sys lists
  it as an untested target, and its README shows the same `not(msvc)` gate.
- Builds: `cargo build --release --features jemalloc` (needs a C toolchain and
  make, because jemalloc-sys runs configure+make), or container
  `--build-arg FEATURES=jemalloc` (composes with `xdp`). On arm64 Linux, a
  non-image build for a host with larger pages than the build host needs
  `JEMALLOC_SYS_WITH_LG_PAGE=16`: a binary built for 4 KiB pages aborts on
  64 KiB-page hosts. The Dockerfile sets it for the image.
- CI: the `Performance` job runs the system build and the jemalloc build as two
  sequential end-to-end arms and reports both; the image matrix builds and
  publishes the `jemalloc` and `xdp,jemalloc` variants on amd64 and arm64, and
  each smoke asserts the startup allocator.
- Runtime tuning uses `_RJEM_MALLOC_CONF` (the prefixed env name), not
  `MALLOC_CONF`; see the malloc_conf arms above. No tuning is shipped.
