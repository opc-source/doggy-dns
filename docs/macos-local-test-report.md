# macOS (Apple Silicon) Local Test Report

- Date: 2026-11-01 (all runs on 2026-11-01, local time CST/UTC+8, log timestamps UTC)
- Branch under test: `perf/ab-bundle` at `a93faf2` (PR #28), binaries from a local
  `cargo build --release` — and, apart from one labelled confirmation round, every
  row below was produced by the `--features xdp` build of that commit, because that
  is what `target/release/doggy-dns` held while the matrices ran (see
  "Which release binary the rows come from")
- Companion record: [xdp-local-test-report.md](xdp-local-test-report.md) — the same
  branch measured on a 2 vCPU Linux host from a CI-built bundle. This report is its
  opposite in every structural way: that host could run the fast path but could not
  compile the workspace; this host compiles and tests everything and cannot run the
  fast path at all.
- No CI artifact was downloaded here. `scripts/fetch_perf_bundle.sh` would have run
  (`gh` and `/sbin/sha256sum` both exist on this macOS), but the bundle contains
  x86_64 Linux ELF binaries, and `scripts/perf_ab.py` itself needs iproute2's `ip`,
  `/proc/<pid>/stat`, `bpftool` and a mounted bpffs — none of which exist on Darwin.

This is a local verification record, not upstream documentation. Raw dnsperf output,
server logs and the run driver live under the gitignored `perf-results/`; the report
quotes each number with the command that produced it.

## Machine specification

| Item | Value |
|---|---|
| Vendor / product | Apple MacBookPro17,1 (13-inch, M1, 2020) |
| CPU | Apple M1, 8 cores = 4 performance (`hw.perflevel0`) + 4 efficiency (`hw.perflevel1`) |
| Memory | 16 GiB (`hw.memsize` 17,179,869,184) |
| OS | macOS 26.6.2 (Build 25G83), `uname` → `Darwin 25.6.0 arm64` |
| Toolchain | rustc 1.99.1 (58cf8f9c3 2026-10-13), cargo 1.99.1 (5f94df478 2026-08-27), stable-aarch64-apple-darwin (pinned by `rust-toolchain.toml`, `channel = "stable"`, not the shell's default host) |
| C build | Apple clang 21.0.0 (clang-2100.3.34.2), target `arm64-apple-darwin25.6.0` |
| dnsperf | 2.16.0, Homebrew bottle `dnsperf 2.16.0_1` (deps: ldns 1.9.2_1, openssl@4 4.1.0, concurrencykit 0.7.2, libnghttp2 1.70.0) |
| Profiling | `/usr/bin/sample` — the sampler the ceiling section uses. There is no `perf` and no `bpftrace` on this host; `/usr/sbin/dtrace` exists and was not used |
| `/tmp` | not used by any benchmark path; runs write under `perf-results/` on the APFS data volume (218 GiB free at the last `df`) |
| Limits | `ulimit -n` 1048575 |
| Loopback | `lo0` mtu 16384 |
| Power | AC, battery 100% charged; system sleep prevented |
| Build cache | `sccache` wraps rustc (`RUSTC_WRAPPER`), so compile times below are warm-cache numbers, not cold-machine numbers |

Practical consequence: this host has real core count to measure against (the Linux
host had two SMT threads on one core), but no BPF at all: the XDP arm, `bpftool`
counters, pinned-map reuse and the kernel-side log level are out of scope here, for
the reasons enumerated next.

## Why the test matrix has no XDP arm

Every one of these was checked on this host while running the suites below:

- **The eBPF program is not compiled here.** `bpf-linker` is absent, `rustup target
  list --installed` has no `bpfel-unknown-none` (only `aarch64-apple-darwin`,
  `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`), so the nested
  `cargo build -Z build-std=core,alloc --target bpfel-unknown-none` that
  `crates/doggy-dns-xdp/build.rs` runs for `xdp/doggy-dns-xdp-ebpf` fails, and
  build.rs writes its documented stand-in. The artifact is a zero-byte file:
  `xdp/target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf` is 0 bytes.
- **The artifact guard passes by skipping, not by asserting.**
  `cargo test -p doggy-dns-xdp --test ebpf_artifact_test -- --nocapture` prints
  `skipping: eBPF ELF is a placeholder for a host without the toolchain` and reports
  `test result: ok. 1 passed`. A green run here says nothing about
  `AYA_LOG_LEVEL` in the object.
- **The userspace crate's kernel surface is compiled out.**
  `crates/doggy-dns-xdp/src/lib.rs` re-exports `program::XdpManager` and
  `XdpCacheMiddleware` only under `#[cfg(target_os = "linux")]`; aya and aya-log are
  `[target.'cfg(target_os = "linux")'.dependencies]`, so on Darwin they are not even
  in the dependency graph.
- **The binary ignores `[xdp]` silently, by construction.** `src/main.rs:22-24`
  selects `src/xdp/linux.rs` under `all(feature = "xdp", target_os = "linux")` and
  `src/main.rs:25-27` selects `src/xdp/stub.rs` otherwise;
  `docs/xdp.md` already states the platform rule ("the fast path is a no-op stub on
  other platforms") and `docs/xdp.md`'s Development section recommends
  `cargo check --workspace --features xdp` on any OS. The stub's `Xdp::enabled()`
  returns `false` and its `init()` does nothing. Observed:
  `cargo build --release --features xdp` succeeds on macOS (it links
  `doggy-dns-xdp`, minus the Linux modules) and a run with
  `[xdp] enabled = true, interface = "lo0"` boots, serves DNS and writes **no** XDP
  record at all — at `RUST_LOG=info` the log is the ordinary startup sequence. The
  two startup warnings that the Linux XDP arm can emit do not fire here, for
  different reasons: the frozen-rotation one is gated on `xdp.enabled() &&
  rotate_answers` (`src/main.rs:129-134`) and the OTLP-metrics one lives inside the
  Linux arm's `start_stats`, behind `xdp.0.as_mut()?` (`src/xdp/linux.rs:68-83`), so
  the stub never reaches either. On Linux the equivalent dead end is loud:
  `src/xdp/linux.rs` logs `failed to initialize XDP fast path, continuing without
  it` at `warn` when `XdpManager::new` returns `Err`.
- **The suite's XDP tests are Linux-only by gate.** `xdp_integration_test` is one
  `#[cfg(target_os = "linux")] mod linux_tests`, so on macOS it reports `running 0
  tests`; the 39 pure-Rust tests in the same crate (cache key/FNV hashing, pin shape,
  stats layout, kernel log-level parsing, cache-entry building) do run here — those
  are the ones that keep the shared-struct layouts honest across the two workspaces.
- **No CI job compiles for Darwin.** Every job in `rust.yml`, `xdp-ci.yml`,
  `nacos-ci.yml`, `codecov.yml` and `perf-bundle.yml` runs on `ubuntu-latest` or
  `ubuntu-22.04`; `image.yml` builds arm64 *Linux* images on native arm64 runners.
  The rows below are therefore the only macOS evidence that exists for `a93faf2`.

Consequence for the matrices: each table here is the Linux report's table with the
`xdp` rows removed, and the throughput ratio is not a headline — there is nothing to
divide by.

## Local build and gates

| Gate | Command | Result |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (default features) | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Lint (`xdp` feature on) | `cargo clippy --all-features --all-targets -- -D warnings` | exit 0 |
| Release build | `cargo build --release --locked` | exit 0, `target/release/doggy-dns` 17,829,920 B (`doggy_dns-77b9a0a5e5ead77d`, sha256 `e4cc2ea8…`), 10m 44s wall / 86.9s user CPU (sccache; the first `cargo` invocation of the session also waited on a package-cache lock) |
| Release build, XDP feature | `cargo build --release --locked --features xdp` | exit 0 in 1m 22s, 17,829,072 B (`doggy_dns-e7ac4f313b2b24b2`, sha256 `bc02f451…`) — and this is the binary the load rows measured (next subsection) |
| Test suite | `cargo test --workspace` | exit 0, **305 passed / 0 failed / 4 ignored** |
| Benchmarks compile | `cargo bench --no-run --workspace` | exit 0, 12 bench executables listed: the four bench files (`handler_bench`, `nacos_bench`, `chain_bench`, `xdp_component_bench`), one `src/lib.rs` harness per crate (7) and the `src/main.rs` harness; 58.41 s for the session's first invocation, which compiled them, and 0.44 s for the re-run from the populated cache quoted here |
| PR CI at the same commit | `gh pr checks 28` | 10/10 pass on `a93faf2` (Lint, Build & Test, Benchmarks, Performance, coverage, XDP fmt/integration/component-bench/stress, Nacos e2e) |

The two lint rows matter more than they look: `.clippy.toml` bans (`unwrap`,
`std::time::Instant`, `std::thread::sleep`, `std::env::set_var`) and the
`--all-features` graph both get exercised on a platform CI never builds for, and the
local toolchain is the same stable 1.99.1 CI uses, so no local-lint drift showed up.

What `a93faf2` contains relative to `main`, so the two reports' "under test" means the
same thing (the Linux report lists this from its bundle manifest at `904508c`; the only
commits between that and here are `275770f`, which commits the `--rust-log` option its
log matrix used from a working tree, and this doc's own commit): the UDP-length verifier
fixes and the aya 0.16 loader/pinned-capacity change, plus `scripts/perf_ab.py`,
`scripts/perf_upstream.c`, `scripts/fetch_perf_bundle.sh` and the `perf-bundle`
workflow. Of that, the macOS rows exercise the userspace half — the runner protocol and
the `--rust-log` accounting — and none of the kernel half.

### Which release binary the rows come from

`cargo build --release` and `cargo build --release --features xdp` share
`target/release/doggy-dns`, and the XDP-feature build was the last one written there
before the matrices ran: its artifact is timestamped 08:36:33, the copy that proves
what the path held was made at 08:37:51, pass 1 ran 08:45–08:51 (its headline rounds
08:46–08:47) and the reported clean pass ran 08:53–08:59. So the honest attribution is:

- **every throughput row in this report measured the `--features xdp` binary**
  (17,829,072 B, sha256 `bc02f451c8a9ad891269ed28f45f13c9dc32b55620116383656698a6ecab9f69`),
  and so did the response-header probe in the smoke table;
- **the plain `--release` binary** (17,829,920 B, sha256
  `e4cc2ea88e4f718c80cc000310815ddc4dcba36facc3f45906fa90cd0d893ead`) **was then
  measured as one headline-shaped confirmation round**: `--tag confirm-plain`, 1 × 30 s,
  `-T 2 -c 2`, `worker_threads = 2`, warn → **132,995 QPS / 0.666 ms / 47.45 CPU-s /
  0 lost / upstream 0**, i.e. +0.33% against the 132,560 median and +0.1% on CPU — both
  well inside the published rounds' own −2.5%/+1.9% spread around that median. Raw
  output: `perf-results/macos-confirm/confirm-plain/`.

That is a measurement rather than an argument: on Darwin the `xdp` feature links the
userspace crate's cache/pin/stats types and a stub loader, and it does not change the
speed of the thing it stubs. It also means the section-by-section numbers below are
comparable to each other even though they were taken with two different builds
present in the same path.

The confirmation round is a second moment in time, not a rerun of the first: the
clean headline rounds ran 08:53:15–08:54:53, the plain `--release` rebuild that
produced its binary finished at 09:12:07 (a 0.32 s relink from the populated cache,
so nothing was compiling inside the measured window) and the round itself ran
09:12:41–09:13:14 — eighteen minutes later. That is separate from the caveat that
does apply to the clean pass: its first round started within a minute of
`cargo bench --no-run` finishing its links (last bench link 08:52:51, round 1's
dnsperf output written 08:53:48). Two independent moments — 132,560 median and
132,995 — agree to 0.33%.

Suite breakdown by crate (integration tests live in `tests/`, so the lib targets
themselves report 0):

| Crate | Passed | Notes |
|---|---:|---|
| `doggy-dns-basic` | 68 | `config_test` 53, `env_test` 8, `net_test` 5, `time_test` 2 |
| `doggy-dns-control` | 28 | http_contract 5, live 3, redact 7, startup 3, state 4, tools 6 |
| `doggy-dns-core` | 30 | handler 21, middleware 2, server 2, wire_golden 5 |
| `doggy-dns-nacos` | 38 | authority 15, mapping 11, watcher 11, alloc_guard 1 |
| `doggy-dns-observability` | 72 | lib 70, error_sampling 1, otlp_export 1 |
| `doggy-dns-plugin` | 27 | chain 12, forward 9, request_outcome 3, wire_query_name 2, system_dns 1 |
| `doggy-dns-xdp` | 39 | cache 10, pin 7, stats 7, kernel_log 6, interface 4, middleware 4, ebpf_artifact 1 (skip-asserts) |
| root `doggy-dns` | 3 | `integration_test`; `nacos_e2e_test` = 4 ignored (needs a live Nacos) |

`forward_test` and `system_dns_test` make real network calls; both passed from this
host, so the macOS resolver path (`/etc/resolv.conf` equivalent through hickory's
system config) is not itself a gap.

## Userspace runtime, same protocol as the Linux report

The load protocol is `scripts/perf_ab.py`'s userspace arm as written, minus what
cannot exist on Darwin:

- config: `[server] listen_addr = "127.0.0.1"`, `port = 15553`, one `forward` plugin
  at `127.0.0.1:15554`, `[xdp] enabled = false`;
- upstream: `scripts/perf_upstream.c` compiled locally
  (`cc -O2 -Wall -Wextra -Werror`, no `-static`, which macOS cannot do), answering
  every A query with `127.0.0.9` TTL 300, its SIGUSR1 query counter in
  `upstream.count`;
- load: `dnsperf -s 127.0.0.1 -p 15553 -d queries.txt -c <clients> -n 100000 -T
  <threads> -l <duration>` over 1000 names (`svc-<i>.perf.doggy.test A`), after a
  warm-up of `-n 2 -l min(10, duration)`;
- server CPU: `ps -o cputime= -p <pid>` before and after the measured window
  (centisecond resolution), the stand-in for `/proc/<pid>/stat`'s utime+stime;
- log accounting: lines and bytes the whole server process wrote over its lifetime —
  warm-up traffic and the smoke probes included, not only the measured window — capped
  at 5,000,000 lines like the runner. The `info` row subtracts the out-of-window lines
  explicitly rather than rounding the difference away;
- the warm-cache guard is the runner's own: upstream queries during the measured
  window must stay ≤ max(20, 1% of completed), else the run is discarded.

Reporting convention for every table below: a "median" is the median of that row's
rounds, computed on the unrounded values from `results.json` (QPS to the hertz, CPU to
the centisecond, latency to the microsecond) and then displayed at the precision in the
column header. Where two rounds put a median exactly between them, the last displayed
digit is whatever the float lands on, so read a final digit in the CPU and latency
columns as ±1 unit — that is why the derived QPS-per-CPU-second column says which
inputs it used.

One deliberate deviation, and it needs saying: **`-c` is pinned to the thread count.**
dnsperf 2.16.0 — this host's version — defaults to one client and prints
`Warning: requested number of threads (-T 2) exceeds number of clients (-c 1),
lowering number of threads`, so `-T` alone does not widen the load. `perf_ab.py`
builds its command from `-s/-p/-d/-n/-T/-l` only, and the CI stress jobs
(`.github/workflows/rust.yml:130`, `xdp-ci.yml:202` and `:212`) pass `-T 4` with no
`-c`. Whether that clamp is what those runners actually see depends on the Ubuntu
dnsperf build — the bundle manifest records 2.9.0, whose default client count was not
checked here. Section "Sender width" below prices the clamp on 2.16.0.

Ports 15553/15554 rather than the runner's 15353/15354: the developer's own long-lived
`./target/debug/doggy-dns config/doggy-dns.toml` instance listens on UDP 15353 (the
local config's `port`), which would have made every row measure that process instead.
The driver now fails fast on an occupied port, which is how the collision was found.

## Headline throughput (userspace only)

`driver.py --tag headline --rounds 3 --duration 30 --threads 2 --clients 2
--worker-threads 2 --queries 1000 --rust-log warn`, i.e. `dnsperf -s 127.0.0.1
-p 15553 -d queries.txt -c 2 -n 100000 -T 2 -l 30` against
`target/release/doggy-dns` on `worker_threads = 2`. This is the Linux report's
"Throughput A/B at warn" with the `xdp` rows and the ratio removed: there is no
fast path to divide by here.

| Round | Arm | QPS | Avg latency (ms) | Lost | Server CPU (s) | Upstream delta | Log lines |
|------:|-----|----:|-----------------:|-----:|---------------:|---------------:|----------:|
| 1 | userspace | 129,201 | 0.684 | 0 | 47.21 | 0 | 0 |
| 2 | userspace | 135,029 | 0.657 | 0 | 47.62 | 0 | 0 |
| 3 | userspace | 132,560 | 0.670 | 0 | 47.40 | 0 | 0 |
| — | **median** | **132,560** | **0.670** | **0** | **47.40** | **0** | **0** |

Readings:

- **11,903,955 queries answered across the three rounds, none lost, 100% NOERROR**
  (round 3: `Response codes: NOERROR 3976882 (100.00%)`, `Average packet size:
  request 40, response 79`, `Latency StdDev 0.000125 s`). The run ends on the
  time limit, not on `-n`, so each round is 30 s of steady state.
- **The warm-cache guard passed with room to spare**: upstream delta 0 in all
  three rounds, so `perf-upstream` saw only warm-up traffic and every measured
  query was answered from hickory's resolver cache. Nothing here measures the
  forward path's cost.
- **Server CPU is 47.40 s per 30 s window ≈ 1.58 of 8 cores.** The process was
  not machine-bound; dnsperf's two senders and `perf-upstream` shared the host.
- **Cross-host, same protocol:** 132,560 vs the Linux host's userspace median
  61,954 QPS = **2.14× on this machine** at the same warn/30 s/warm-cache settings.
  "Same protocol" is doing a lot of work in that sentence and it is not fully earned:
  this row sent `-T 2 -c 2`, the Linux row sent `-T 2` with no `-c` (its runner
  doesn't pass one), and neither dnsperf 2.14.0 — that host's version — nor the 2.9.0
  in the bundle manifest was checked here for the client clamp the protocol section
  describes. If 2.14.0 clamps like 2.16.0, the Linux number is a single-client row and
  the comparable local figure is the 124,193 QPS `-c 1` row instead — a 2.00× ratio
  rather than a 2.14× one. Treat the ratio as an order-of-magnitude host comparison,
  not a branch improvement, and note
  it is the weakest number in this report (see Limits).

## Userspace log-level matrix

2 rounds × 10 s per level (`--rounds 2 --duration 10`), `-T 2 -c 2`,
`worker_threads = 2`, `RUST_LOG` from `--rust-log`. Same shape as the Linux
report's matrix, minus the two `xdp` rows and the kernel-side level discussion.

| RUST_LOG | QPS median | Rounds | Avg latency (ms) | Lost | Server CPU (s / 10 s) | Upstream delta | Log lines | Log size |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| error | 134,717 | 134,538 / 134,895 | 0.657 | 0 | 15.89 | 0 | 0 | 0 B |
| warn | 135,131 | 134,783 / 135,479 | 0.653 | 0 | 15.89 | 0 | 0 | 0 B |
| info | 43,550 | 42,885 / 44,214 | 2.256 | 0 | 14.20 | 0 | 1,312,744 | 720,867,312 B |

Round-level log accounting for `info`: 1,292,775 lines / 709,894,608 B in round 1
(428,915 completed) and 1,332,713 lines / 731,840,015 B in round 2 (442,232
completed).

Readings:

- **error and warn are indistinguishable.** Their medians are 0.31% apart (134,717 vs
  135,131) while the rows' own round-to-round spreads are 0.27% and 0.52% peak-to-peak,
  so the gap is smaller than the noise inside one row. Both wrote zero bytes during
  their measured window — warn is a clean operating point on this host too,
  with no query-path record at that level.
- **info costs 3.10×** (135,131 → 43,550 QPS) and raises mean latency 3.45×
  (0.653 → 2.256 ms). Per request the cost is exactly **3 info lines, and both files
  reconcile to it**: round 1's 1,292,775 lines are 11 startup/shutdown records +
  3 × 430,920 resolved `A` requests + 2 × 2 for the `AAAA`/`TXT` probes (which
  `hickory_server::server` logs but the handler does not resolve), and those 430,920
  are 428,915 measured + 2,000 warm-up + 5 probe queries (3 from the smoke digs, 2
  readiness). Round 2 is the same shape with no smoke digs: 1,332,713 = 11 + 3 ×
  444,234, where 444,234 = 442,232 measured + 2,000 warm-up + 2 readiness. The three
  lines are `hickory_server::server: query:…`, its `request:… answers=1 duration_ms=…`
  record and `doggy_dns_core::handler: query resolved`. So the whole-file tally in the
  `Log lines`/`Log size` columns — the runner counts the file, not the window — sits
  ~0.5% above the measured traffic, and **549.1 bytes per line** holds in both rounds
  (709,894,608 ÷ 1,292,775 = 549.12; 731,840,015 ÷ 1,332,713 = 549.14). Dividing the
  Linux report's published `info` row the same way gives 3.0 lines per request there
  too (866,531 ÷ 3 ≈ 288,844 against 286,800 completed + 2,000 warm-up) and 549.1 B per
  line (475,851,059 ÷ 866,531); see the follow-up list, because that report says 18.
  Same format string, same per-line cost on both platforms; the throughput hit differs
  (2.4× on Linux, 3.1× here) because the two hosts spend their headroom differently,
  and a cross-host ratio at one config is not a platform finding.
- **On this host the log path is not CPU-bound.** `info` measured *lower* server
  CPU than `warn` (14.20 vs 15.89 s per 10 s window) while completing a third as
  many queries: the cost shows up as wall-clock latency, not as cycles. 720 MB in
  10 s ≈ 72 MB/s to the redirected log file on APFS. (On the Linux host the same
  level raised CPU, 10.60 → 14.36 s — the sign differs between hosts, so this is
  reported as observed rather than generalised.)

## Worker-thread matrix (`worker_threads` 1 → 8)

The Linux report closed its own version of this section with a scope limit: "on a
2 vCPU host … 2 workers is already the host ceiling, so this matrix can only say
'no gain up to `nproc`'. A many-core host … may move the userspace ceiling; that
is not measured here." This host has 8 logical cores, so the range is `hw.ncpu`,
and the curve past 2 is measured.

2 rounds × 10 s per row, `-T 8 -c 8` (senders and threads held at the same width in
every row, so the *server* setting is the only thing that varies — the sender-width
section below prices what that width costs against a narrower load), `RUST_LOG=warn`.

| worker_threads | QPS median | Rounds | Avg latency (ms) | Server CPU (s / 10 s) | Lost | Upstream delta | Log lines | QPS per CPU-second |
|---------------:|-----------:|-------:|-----------------:|----------------------:|-----:|---------------:|----------:|-------------------:|
| 1 | 93,079 | 93,133 / 93,024 | 1.060 | 9.48 | 0 | 0 | 0 | 9,824 |
| 2 | 116,748 | 118,075 / 115,422 | 0.828 | 15.23 | 0 | 0 | 0 | 7,663 |
| 4 | 107,137 | 107,066 / 107,208 | 0.908 | 22.27 | 0 | 0 | 0 | 4,812 |
| 8 | 103,267 | 103,534 / 102,999 | 0.942 | 23.50 | 0 | 0 | 0 | 4,395 |

Readings:

- **Throughput peaks at `worker_threads = 2`, then falls.** 1 → 2 is +25.4%
  (93,079 → 116,748); 2 → 4 is −8.2%; 4 → 8 is −3.6%. Every row's own two rounds sit
  within ±1.2% of its median (widest: worker 2 at 118,075 / 115,422, ±1.1%), so both
  declines are outside run-to-run noise, if modest — this is a plateau past 2, not a
  cliff.
- **The 1 → 2 gain is a second core, not a second receive path.** At 1 worker the
  process burned 9.48 CPU-s per 10 s window = 0.95 of one core, i.e. it was
  thread-bound; at 2 it used 1.52 cores. `crates/doggy-dns-core/src/server.rs:20`
  still binds one UDP socket, so the receive→answer loop remains one task and the
  per-request work around it is what spreads — which is why the same 1 → 2 step
  that gained +2.0% on a 1-core/2-SMT Linux host gains +25% here.
- **Per-core efficiency falls monotonically** (last column, derived: median QPS ÷
  median CPU-s per window, both from the unrounded round values — dividing the rounded
  figures printed in this table instead moves the four cells by 6, 3, 1 and 1
  QPS/CPU-s). 8
  workers buy 2.35 cores of throughput-cost for 11.5% less output than 2 workers. The
  default `worker_threads = 1` leaves ~25% on the table on this host; 2 is the measured
  optimum for raw UDP reply rate here, which is the value both reports' headline runs
  use.
- **Read this row against the headline:** `worker_threads = 2` gives 116,748 here
  but 132,560 in the headline section, because this matrix loads with 8 senders
  and the headline with 2. The sender-width section below prices that difference;
  the matrix is only comparable within itself.
- **Not an XDP statement**: these rows are the userspace arm only, and the answer
  rotation/cache behaviour is identical across them.
- **What turns the curve is measured, not guessed**: the "Where the ceiling sits" section
  attributes per-thread CPU and a call-graph profile of this same binary, and finds the
  serial part is one socket loop task that `worker_threads` can migrate but not split.

## Sender width (dnsperf `-c`) at `worker_threads = 4`

The protocol section records that `perf_ab.py` passes `-T` and never `-c`, which on
dnsperf 2.16.0 pins the real sender count to one. This experiment measures what that
clamp costs at a fixed server: 2 rounds × 10 s, `RUST_LOG=warn`, `worker_threads = 4`,
`-c` set equal to `-T`.

| `-c` / `-T` | QPS median | Rounds | Avg latency (ms) | Server CPU (s / 10 s) | Lost | Upstream delta |
|------------:|-----------:|-------:|-----------------:|----------------------:|-----:|---------------:|
| 1 | 124,193 | 124,244 / 124,142 | 0.522 | 24.03 | 0 | 0 |
| 2 | 117,832 | 117,889 / 117,775 | 0.740 | 22.94 | 0 | 0 |
| 4 | 109,628 | 108,383 / 110,873 | 0.882 | 22.63 | 0 | 0 |
| 8 | 106,275 | 106,853 / 105,696 | 0.915 | 22.33 | 0 | 0 |

Readings:

- **QPS falls monotonically as the offered load widens** (−5.1% at 2 senders,
  −11.7% at 4, −14.4% at 8; the widest inside-row spread here is 1.2%, on the
  `-c 4` row) while latency rises 0.52 → 0.92 ms. More senders on the same
  loopback machine buy less throughput.
- **Server CPU is flat** (22.3–24.0 s per 10 s window, i.e. ~2.3 cores) across the
  whole width, so this is not the server running out of workers; it is the shared
  host — dnsperf's own threads compete with the server, and 8 senders were not the
  limiter at any width the server could use.
- **Direction for reading the runner's numbers:** *if* the CI clamp behaves as it
  does on this host, the runner's and the stress jobs' rows are single-client — and
  single-client is the **fastest** row here. So `perf_ab.py` is not understating the
  userspace ceiling by forgetting `-c`; what it misstates is the load *shape*: a
  "-T 4" run is one client at a time, which no real client population is. Checking
  2.9.0's default on the CI image is a one-command follow-up.

## Where the ceiling sits: one loop task and the syscall pair it owns

The worker-thread matrix gives the shape — +25.4% at 2 workers, then −8.2% and a further
−3.6% step by step — without explaining it. This section attributes the time instead of
inferring it, with three measurements of the same process under the same load:

- **per-thread CPU**: `ps -M -p <pid>` twice, 8 s apart inside one 20 s load window; each
  thread's STIME+UTIME delta ÷ 8 s = cores. On Darwin the per-thread rows carry no
  USER/PID column, so the fields shift; the row that does carry the command path is the
  process's main thread, and it never inflates a worker. Fourteen of the sixteen snapshots
  read `0:00.00 / 0:00.00`; the other two are the `worker_threads = 8` pair, which both read
  `0:00.00 / 0:00.01` — the same value in both, so its delta over every 8 s window is 0.00
  CPU-s and skipping that row costs nothing.
- **an open-loop control**: the same sweep with dnsperf `-q 2000` (2.16.0 documents `-q` as
  "maximum number of queries outstanding", default 100), to separate "the senders stopped
  pushing" from "the server stopped pulling".
- **a call-graph profile**: `/usr/bin/sample <pid> 8` under load, which samples every thread
  of the process — parked ones included — and prints both a per-thread call tree and a
  top-of-stack histogram.

All three use the `--features xdp` release binary the matrices measured (sha256
`bc02f451…`), the same `queries.txt`, `RUST_LOG=warn`, a 2-query warm-up before each
window, and a fresh `perf-upstream` per sweep on a separate port so nothing collides:
15573/15574 for the closed sweep (`-l 20`), 15575/15576 for the `-q 2000` sweep (`-l 20`),
15577/15578 for the profile (`-l 25`, sampled for 8 s inside it). Every table and the
diagram below are the output of `perf-results/macos-sample/analyze_probe.py` over the
captures in that directory.

### Per-thread CPU, closed loop (dnsperf default `-q 100`)

| worker_threads | QPS | Avg latency (ms) | In flight (QPS × latency) | Lost | Process cores | Busiest thread | Per-thread cores | CPU µs/query |
|---------------:|----:|-----------------:|--------------------------:|-----:|--------------:|---------------:|:-----------------|-------------:|
| 1 | 94,196 | 1.048 | 98.7 | 0 | 0.95 | 0.95 | 0.95 | 10.1 |
| 2 | 118,412 | 0.815 | 96.5 | 0 | 1.53 | 0.77 | 0.77, 0.76 | 12.9 |
| 4 | 105,156 | 0.923 | 97.1 | 0 | 2.13 | 0.53 | 0.53, 0.53, 0.53, 0.53 | 20.2 |
| 8 | 104,102 | 0.936 | 97.4 | 0 | 2.33 | 0.30 | 0.30, 0.29, 0.29, 0.29, 0.29, 0.29, 0.29, 0.29 | 22.3 |

Readings:

- **The matrix reproduces.** Against its medians (93,079 / 116,748 / 107,137 / 103,267)
  these rows differ by +1.2%, +1.4%, −1.8%, +0.8%, and process cores here match the
  matrix's CPU-s per 10 s (9.48 / 15.23 / 22.27 / 23.50 ÷ 10) by 0.2–4.4%. The plateau
  past 2 is not an artefact of the shorter window.
- **`worker_threads = 1` really is one thread**: `ps -M` has exactly one thread row, it
  holds 0.95 of a core, and the main thread burned 0.00 CPU-s. That one thread answers
  94,196 QPS.
- **Nothing is saturated from 2 workers on.** At 8 workers the busiest of the nine threads
  uses 0.30 of a core and the process as a whole uses 2.33 of the 8 available — yet QPS is
  12.1% below the 2-worker row. Idle capacity cannot be the ceiling.
- **Per-query CPU doubles while QPS goes backwards** (last column, derived: process cores
  ÷ QPS): 10.1 → 12.9 → 20.2 → 22.3 µs. Rows 4 and 8 burn 2.2× and 2.5× row 1's CPU to
  return 11.2% and 12.1% less than row 2. That is the whole question: where does the extra
  CPU go?

### Control: the senders were never the limiter

The same four rows, `-q 2000` added, one 20 s window per row on port 15575 ("in flight" is
derived as QPS × latency, not read from a client-side counter):

| worker_threads | QPS (`-q 2000`) | Avg latency (ms) | In flight | Lost | Process cores | CPU µs/query |
|---------------:|----------------:|-----------------:|----------:|-----:|--------------:|-------------:|
| 1 | 92,632 | 21.520 | 1,993.4 | 0 | 0.95 | 10.3 |
| 2 | 117,995 | 16.900 | 1,994.1 | 0 | 1.55 | 13.1 |
| 4 | 105,251 | 18.957 | 1,995.2 | 0 | 2.16 | 20.5 |
| 8 | 104,409 | 19.103 | 1,994.5 | 0 | 2.35 | 22.6 |

Readings:

- **20× the offered concurrency, same throughput**: in flight goes 97 → 1,995 at `worker_threads = 8`
  and QPS moves +0.3% (104,102 → 104,409), with 0 lost in both. The four closed-loop rows
  are not latency-starved, and raising the client side does not lift the ceiling.
- **Latency 0.936 → 19.103 ms at the same QPS** is the signature of a rate-limited server:
  Little's law says the extra ~1,897 in-flight queries have to be held somewhere while the
  server works through them at the same rate. Which buffer holds them is not observed here —
  nothing read socket or `netstat` statistics, so "the kernel receive queue" is an inference,
  and it is disclaimed in Limits. What the control does establish is the server-side half:
  20× the client concurrency, +0.3% QPS, 0 lost, so the senders were never the limit.

### What the time is spent on (`sample`, 8 s under load)

| worker_threads | QPS while profiling | thread samples | threads | main-thread samples | runtime-thread samples | runtime parked | runtime on-CPU | `sendto`+`recvfrom`, % of on-CPU |
|---------------:|--------------------:|---------------:|:--------|--------------------:|-----------------------:|---------------:|---------------:|---------------------------------:|
| 1 | 91,080 | 12,038 | 1 main + 1 runtime | 6,114 | 5,924 | 0.1% (7) | 99.9% (5,917) | 56.3% |
| 8 | 97,303 | 46,555 | 1 main + 8 runtime | 5,189 | 41,366 | 72.1% (29,824) | 27.9% (11,542) | 36.9% |

`sample` records parked threads too, so the main thread — which sits in `runtime.block_on`'s
park for the whole run (`src/main.rs:68-71` builds the multi-thread runtime named
`doggy-dns-runtime`, the label on every other row, and `:79` blocks on it) — accounts for
one thread's worth of samples at every setting; at `worker_threads = 1` it is 6,114 of the
6,121 parked ticks, at 8 it is 5,189 of 35,013, and its entire call tree is that park (all
6,114 wt=1 ticks run `tokio::runtime::park::Inner::park` → `parking_lot::Condvar`). The
profiling run is its own measurement: its QPS (91,080 / 97,303) sits a little under the
closed-loop rows above because the profiler attaches to the process it samples, so the two
rows are compared with each other, not with those tables.

Two columns are derived rather than read. `sample` prints one Call-graph header per thread,
and every runtime thread's header carries the same count as the main thread's — 6,114 at
wt=1, 5,189 at wt=8 — while the top-of-stack histogram drops ticks below its "when >= 5"
cut, so `thread samples` is the histogram sum (12,038 / 46,555) and `runtime-thread samples`
is that sum less the main thread's header count: 5,924 against a header of 6,114 for the one
wt=1 worker, and 41,366 against 8 × 5,189 = 41,512 at wt=8. The percentages are taken over
the derived set; nothing in this section divides by the sample interval (see Limits).

Partitioning the on-CPU runtime-thread samples by leaf symbol:

| category | wt=1 samples | wt=1 % of 5,917 | wt=8 samples | wt=8 % of 11,542 |
|---|---:|---:|---:|---:|
| syscall: `__sendto` | 2,875 | 48.6% | 3,404 | 29.5% |
| syscall: `__recvfrom` | 455 | 7.7% | 860 | 7.5% |
| alloc / free / memmove / memset | 929 | 15.7% | 2,559 | 22.2% |
| kernel clock (`mach_absolute_time`) | 249 | 4.2% | 594 | 5.1% |
| server code (everything else) | 1,409 | 23.8% | 4,125 | 35.7% |

The five rows partition the set exactly (2,875+929+455+249+1,409 = 5,917 and
3,404+2,559+860+594+4,125 = 11,542). Largest individual leaves besides the syscall pair:
`mach_absolute_time` 238 and `_platform_memmove` 237 at wt=1; `_xzm_xzone_malloc_tiny` 714,
`_platform_memmove` 614, `_xzm_free` 582, `mach_absolute_time` 563 at wt=8, and among the
named server frames `ServerContext<DoggyDnsHandler>::handle_raw_request` 164,
`Resolver<TokioRuntimeProvider>::lookup::<Name>` 156 and `CachingClient<…>::clone` 149
(those three labels are shorthand for the mangled symbols quoted in `analysis.txt`).

Readings:

- **At one thread, 72.0% of its CPU is mechanics, not DNS logic** (48.6% `sendto` + 7.7%
  `recvfrom` + 15.7% allocator/copy); the "server code" residual is 23.8%.
- **The syscall pair is not what grows.** Its tick count rises 1.28× (3,330 → 4,264)
  while everything else rises 2.81× (2,587 → 7,278), which is why its share falls
  56.3% → 36.9% even though `sendto` itself goes 2,875 → 3,404 ticks. The extra CPU that
  8 workers buy shows up in allocation, copying, timers and hickory/tokio frames instead of
  in socket work. This section locates that growth; it does not price it, so "handoff and
  coordination overhead" would be a reading of the shape, not a measurement.
- **The "server code" row is a residual, not a measurement of DNS logic.** `sample` prints
  raw symbols; the classifier recognises libc leaves and buckets every unrecognised
  rustc-mangled frame (hickory/proto/resolver/tokio) into it. The three named frames above
  are inside that bucket, so 35.7% is an upper bound on "answer construction", not a
  breakdown of it.

### Which task owns the socket

The profile's frame ancestry answers the structural question directly: how many syscall
samples were taken while polling the socket loop (a `handle_udp` frame on the stack)
rather than a per-request future.

| worker_threads | runtime threads carrying syscall samples | `sendto` | `recvfrom` | of those, under a `handle_udp` frame | per-thread `sendto` range |
|---------------:|-----------------------------------------:|---------:|-----------:|-------------------------------------:|:--------------------------|
| 1 | 1 of 1 | 2,875 | 456 | 3,331 = 100% | 2,875 |
| 8 | 8 of 8 | 3,404 | 860 | 4,264 = 100% | 404–487 |

(`recvfrom` reads 456 here and 455 in the table above: `sample` prints a top-of-stack
entry only "when >= 5", so the single tick at `__recvfrom + 28` is in the full-depth call
tree and not in the histogram.)

Readings:

- **Every socket syscall in both runs belongs to the loop task** — 3,331/3,331 and
  4,264/4,264. There is no second receive path, and no send outside the loop: a resolver
  forward (a cache miss) `sendto`s on the upstream socket from the per-request future, i.e.
  outside any `handle_udp` frame, and none of the 4,264 ticks is there. The whole sampled
  interval is cache-warm by construction: `sample` starts 6 s into a 25 s window, and the
  misses the 2-query warm-up leaves behind are ~1,000 names (the query file's count), i.e.
  a hundredth of a second at this rate. This pass did not prove it — `thread_probe.sh` and
  its siblings pass `--count-file` but never send `perf-upstream` the SIGUSR1 that appends a
  dump, so the three `upstream*.count` files here hold only the startup seed `0` and the
  warmth claim rests on that structure. The guard is borrowed from the matrix, whose headline
  rounds are 30 s each and report `Upstream delta` 0 in all three, so a 20 s or 25 s window on
  the same 1,000-name file cannot be less warm. Read that way, the 10.1 µs/query at
  `worker_threads = 1` is the cost of answering from the cache, not of fetching.
- **The loop did not parallelise, it migrated.** At `worker_threads = 8` the same single
  future is polled wherever tokio puts it, and its `sendto` ticks land on all 8 workers
  (404–487 each). One task, now crossing cores — and the measured growth is in the
  non-syscall categories (2.81×), not in the socket pair (1.28×).

### The shape, annotated

The left column is the path one query takes; the right column is this section's
measurements against that path. Generated with the tables above:

```text
  one doggy-dns process, loopback (lo0), warm resolver cache

  dnsperf  x 8 senders                                          What the rows say about this shape
  -------------------                                           ----------------------------------
      sendto(query)                                             worker_threads = 1: 94,196 QPS on 0.95 cores, and `ps -M`
            |                                                     has 1 thread row to attribute them to; `sample` agrees:
            v                                                     the worker is on-CPU for 99.9% of its 5,924 samples, and
  kernel: lo0 socket                                              6,114 of the 6,121 parked ones are the main thread.
  receive queue                                                   One thread is the whole server, and its loop
            |                                                     task owns the socket.
        recvfrom()                                              worker_threads = 8: 104,102 QPS on 2.33 cores, busiest thread
      1 datagram/poll                                             0.30 cores, and 72.1% of the runtime-thread samples park.
            |                                                     Nothing is saturated, yet QPS stayed below the 118,412 that two
            v                                                     threads reached.
   [handle_udp task]                                            The loop does not parallelise, it migrates: 100% of the
     ONE per socket                                               sendto+recvfrom samples sit under a `handle_udp` frame, and
            |                                                     those 2,875 sendto at wt=1 become 3,404 spread over
       spawn task                                                 all 8 workers at wt=8 (404-487 each) --
            |                                                     one task still, now crossing cores.
            v                                                   Per-query CPU 10.1 us -> 22.3 us: the extra threads bought
  [per-request task]                                              allocators, not throughput.
    x QPS, on any                                               On-CPU at wt=8: sendto 29.5% and recvfrom 7.5%
       worker                                                     = 36.9% of running samples in the syscall pair this loop owns;
            |                                                     alloc/free/copy is another 22.2%.
    AuthorityChain                                              Concurrency is not the lever either: at wt=8 -q 100 holds 97
     -> forward                                                   in flight and -q 2000 holds 1995 for 104,409 vs 104,102 QPS,
     -> moka hit                                                  0 lost -- the senders were never the limit.
     -> clone Msg
            |
            v
   reply pushed to
   the ONE mpsc
            |
            v
   drained by the
   handle_udp task
     BEFORE recv
            |
       sendto(reply)
            |
            v
    back to dnsperf
```

### Why `worker_threads` cannot reach this

Three properties of the receive path, all in code we do not own, set what the matrices
could possibly show:

- **One UDP socket → one loop task.** `crates/doggy-dns-core/src/server.rs:20-23` binds a
  single `UdpSocket` and hands it to `Server::register_socket`, whose body is
  `join_set.spawn(handle_udp(socket, …))` (`hickory-server-0.26.3/src/server/mod.rs:102-104`),
  the receive loop at `:449-457`. Raising `worker_threads` adds threads that poll *other*
  tasks; it never adds a receive path.
- **One datagram per poll, and sends first.** `UdpStream::poll_next`
  (`hickory-net-0.26.3/src/udp/udp_stream.rs:154-190`) drains the outbound queue before
  receiving, with the design intent in its own comments: "this will not accept incoming
  data while there is data to send / makes this self throttling" (`:159-160`), "sending
  will be preferred over receiving" (`:166`), "For QoS, this will only accept one message"
  (`:181`). So the reply that the spawned per-request task pushed into the socket's
  `BufDnsStreamHandle` channel (created at `:126-127`) is `sendto`'d by the *loop* task on
  its next poll, immediately before the next `recvfrom` — which is what the 100%-under-`handle_udp`
  attribution above measures.
- **One task per request, and per-request allocation.** The loop spawns
  `handle_raw_request` per datagram (`mod.rs:484`, reaped at `:664`), so the answer
  construction can run on any worker while the syscall pair cannot. Each request also
  allocates on arrival: `poll_next` copies the datagram
  out of a 4 KiB stack buffer (`MAX_RECEIVE_BUFFER_SIZE`, `hickory-net-0.26.3/src/udp/mod.rs:30`)
  into a fresh `Vec` (`udp_stream.rs:185-188`), and `forward`/`native` answers still walk
  `lookup.iter().cloned().collect()` (`crates/doggy-dns-core/src/handler.rs:514-520` — the
  owned-`Vec` fast path covers the `nacos` authority's `LookupRecords::Section`, not the
  two resolver-backed authorities). Together those are the `alloc / free / memmove / memset`
  row, 15.7% of one thread's CPU and 22.2% of the 8-worker pool.

Put together: the ceiling on this host is ~94k QPS at `worker_threads = 1` because one
thread is on-CPU for 99.9% of its samples and 72.0% of them sit in the recv/send pair plus
the allocator — 10.1 µs of CPU per query, on a machine with 8 cores. What 1 → 2 buys is a
second thread for the spawned per-request tasks to run on: the process stops being
thread-bound (0.95 → 1.53 cores) and QPS rises 25.4%. No worker becomes a dedicated receive
thread — both land at 0.77 cores — so the gain is "the handlers left the loop's thread",
not "the loop got its own". Past 2 the only new capacity is threads to migrate that one loop
across, per-query CPU doubles, 72.1% of worker samples park, and throughput lands below the
2-worker row.
The knob that would move it is fewer syscalls and fewer allocations per reply — batching
on the socket (`recvmmsg`/`sendmmsg`, UDP GSO) or a path that answers in the kernel, which
is what the XDP arm is for and why this section stops at attribution: this SDK's
`libsystem_kernel.tbd` exports `_sendto`/`_recvfrom`/`_recvmsg` and no `_recvmmsg` or
`_sendmmsg`, and AF_XDP is Linux-only, so the fix cannot be measured here. The structural
variant that does add receive paths, per-worker `SO_REUSEPORT` listeners, is a documented
Non-Goal of the perf round rather than something measured here:
`docs/superpowers/specs/2026-10-29-userspace-perf-design.md:52-57` names this same
one-task-per-socket loop, calls fan-out "the only change plausibly worth an order of
magnitude", and defers it pending the end-to-end baseline; `:58-60` defers replacing
hickory's UDP path (own receive loop, no `SerialMessage` copy, no per-response channel hop,
`recvmmsg`) as a subsystem restructure. A throwaway multi-listener build was tried on the
Linux host during that round and discarded without landing, so no artifact in this repo
reproduces its numbers and this report makes no claim about them. Operationally the
conclusion is the one the matrix already gave: `worker_threads = 2` on this machine, and the
default of 1 leaves ~25% on the table.

## Functional smoke on macOS

The load driver runs five queries before any traffic, in round 1 of the headline
tag (`dig @127.0.0.1 -p 15553 +noall +answer +short …`); `driver.py` captures
stdout+stderr, so a timeout would have landed in those cells as error text rather
than as empty. An empty `+short` is ambiguous between NODATA and an error status,
so the same five behaviours were then probed against a fresh instance on ports
15563/15564 with `+noall +comments +answer`, with the whole exchange saved to
`perf-results/macos-probe/probe.out` — the `Observed` column quotes that file
verbatim, and it names the exact binary bytes (sha256 `bc02f451…`, the same as the
matrices ran).

| # | Query | Observed (`probe.out`) | Result |
|---|---|---|---|
| 1 | `p4.perf.doggy.test A` | `status: NOERROR`, `flags: qr rd ra; QUERY: 1, ANSWER: 1, AUTHORITY: 0`, `p4.perf.doggy.test. 300 IN A 127.0.0.9` | pass |
| 2 | `p4.perf.doggy.test A` again | same header, same `300 IN A 127.0.0.9` | pass |
| 3 | `p4.perf.doggy.test AAAA` | `status: NOERROR`, `QUERY: 1, ANSWER: 0, AUTHORITY: 0` — NODATA, not SERVFAIL | pass |
| 4 | `p5.perf.doggy.test TXT` | `status: NOERROR`, `ANSWER: 0` — NODATA | pass |
| 5 | `p6.perf.doggy.test A +tcp` | `status: NOERROR`, `ANSWER: 1`, `300 IN A 127.0.0.9` | pass |
| 6 | server log at warn | `perf-results/macos-probe/server.log` 0 B; the headline round's `r1.server.log` also 0 B | pass |

The headline round's own `+short` cells (`smoke.perf.doggy.test` A, A, AAAA;
`txt.perf.doggy.test` TXT; `tcp.perf.doggy.test` A over TCP) recorded the same five
values in `perf-results/macos-load-clean/headline/results.json`: `127.0.0.9`,
`127.0.0.9`, empty, empty, `127.0.0.9`.

The probe is also the warm-cache proof for the arm every throughput row measures:
`perf-upstream`'s SIGUSR1 counter dumped **4** for those five queries
(`upstream.count` = `0 4` — seed, then dump), so exactly one — the repeated `A` —
was answered from the resolver cache instead of being forwarded. That is the
behaviour the `-n 100000` passes rely on, and it is why the warm-cache guard is
worth stating as a guard rather than an assumption.

What this does *not* establish: the Nacos authority (no server here), the
negative-TTL and pinned-map behaviours of the XDP cache (not attachable), and the
control-plane endpoints (exercised by `doggy-dns-control`'s 28 tests, not by this
smoke). Those are the rows the Linux report's functional matrix could fill and
this one cannot.

## Reproduce

The reported pass is `perf-results/macos-load-clean/` (`--out-root` below). The
identical commands were first run into `perf-results/macos-load/`; that pass is
also kept locally, and its headline rounds are the contaminated ones described in
Limits.

```bash
# 0. local build, no CI bundle. The matrices measured the `--features xdp` bytes
#    (see "Which release binary the rows come from"), so build those first and copy
#    them aside before anything else can overwrite the shared path.
cargo build --release --locked --features xdp   # target/release/doggy-dns = the rows' bytes
mkdir -p perf-results/macos-driver/bin
cp target/release/doggy-dns perf-results/macos-driver/bin/doggy-dns-xdp-feature
cargo bench --no-run --workspace                # the compile-only gate
cc -O2 -Wall -Wextra -Werror -o perf-results/macos-driver/bin/perf-upstream \
   scripts/perf_upstream.c

# 1. headline: 3 rounds x 30 s, dnsperf -T 2 -c 2, RUST_LOG=warn
#    (round 1 also runs the five dig queries of the smoke table before load)
python3 perf-results/macos-driver/driver.py --tag headline \
  --binary target/release/doggy-dns \
  --upstream-binary perf-results/macos-driver/bin/perf-upstream \
  --port 15553 --upstream-port 15554 --out-root perf-results/macos-load-clean \
  --rounds 3 --duration 30 --threads 2 --clients 2 --worker-threads 2 \
  --queries 1000 --rust-log warn

# 2. log-level matrix: error / warn / info, 2 rounds x 10 s
for lvl in error warn info; do
  python3 perf-results/macos-driver/driver.py --tag "level-$lvl" \
    --binary target/release/doggy-dns \
    --upstream-binary perf-results/macos-driver/bin/perf-upstream \
    --port 15553 --upstream-port 15554 --out-root perf-results/macos-load-clean \
    --rounds 2 --duration 10 --threads 2 --clients 2 --worker-threads 2 \
    --queries 1000 --rust-log "$lvl"
done

# 3. worker-thread matrix: 1..8 (this host's hw.ncpu), senders pinned to 8
for wt in 1 2 4 8; do
  python3 perf-results/macos-driver/driver.py --tag "worker-$wt" \
    --binary target/release/doggy-dns \
    --upstream-binary perf-results/macos-driver/bin/perf-upstream \
    --port 15553 --upstream-port 15554 --out-root perf-results/macos-load-clean \
    --rounds 2 --duration 10 --threads 8 --clients 8 --worker-threads "$wt" \
    --queries 1000 --rust-log warn
done

# 4. sender width at worker_threads = 4, 2 rounds x 10 s
for c in 1 2 4 8; do
  python3 perf-results/macos-driver/driver.py --tag "clients-$c" \
    --binary target/release/doggy-dns \
    --upstream-binary perf-results/macos-driver/bin/perf-upstream \
    --port 15553 --upstream-port 15554 --out-root perf-results/macos-load-clean \
    --rounds 2 --duration 10 --threads "$c" --clients "$c" --worker-threads 4 \
    --queries 1000 --rust-log warn
done

# 5. the response-header probe behind the smoke table (ports 15563/15564, so it
#    cannot collide with a load pass). Fresh names per behaviour: a name queried
#    twice is the cache-hit case, so the other rows must not reuse it.
mkdir -p perf-results/macos-probe
sed 's/15553/15563/; s/15554/15564/' \
    perf-results/macos-load-clean/headline/userspace.toml \
    > perf-results/macos-probe/probe.toml
./perf-results/macos-driver/bin/perf-upstream --port 15564 \
    --count-file perf-results/macos-probe/upstream.count &
RUST_LOG=warn ./target/release/doggy-dns perf-results/macos-probe/probe.toml \
    > perf-results/macos-probe/server.log 2>&1 &
dig @127.0.0.1 -p 15563 p4.perf.doggy.test A       +noall +comments +answer
dig @127.0.0.1 -p 15563 p4.perf.doggy.test A       +noall +comments +answer
dig @127.0.0.1 -p 15563 p4.perf.doggy.test AAAA    +noall +comments +answer
dig @127.0.0.1 -p 15563 p5.perf.doggy.test TXT     +noall +comments +answer
dig @127.0.0.1 -p 15563 p6.perf.doggy.test A +tcp  +noall +comments +answer
kill -USR1 %1; cat perf-results/macos-probe/upstream.count   # -> "0 4"
#    (SIGUSR1 is signal 30 on Darwin, 10 on Linux x86 — send it by name, and note
#     `kill -l USR1` differs between the two platforms)

# 6. the cross-build check: cargo writes the plain binary back to
#    target/release/doggy-dns, so copy it aside and repeat one headline round
cargo build --release --locked
cp target/release/doggy-dns perf-results/macos-driver/bin/doggy-dns-plain
python3 perf-results/macos-driver/driver.py --tag confirm-plain \
  --binary perf-results/macos-driver/bin/doggy-dns-plain \
  --upstream-binary perf-results/macos-driver/bin/perf-upstream \
  --port 15553 --upstream-port 15554 --out-root perf-results/macos-confirm \
  --rounds 1 --duration 30 --threads 2 --clients 2 --worker-threads 2 \
  --queries 1000 --rust-log warn

# 7. the ceiling section: per-thread CPU, the -q 2000 control, then a `sample`
#    profile. Three one-off bash loops over the same binary, the same queries.txt
#    and a config rewritten from headline/userspace.toml (worker_threads, port,
#    upstream port). They are gitignored with the rest of perf-results/, so the
#    four lines that matter are here:
#      server:   RUST_LOG=warn <doggy-dns-xdp-feature> wt$wt.toml
#      load:     dnsperf -s 127.0.0.1 -p $PORT -d queries.txt -n 100000 -l 20 -T 8 -c 8
#                (add `-q 2000` for the control; the profile pass uses -l 25)
#      CPU:      ps -M -p $server_pid at t+6 and t+14      -> wt$wt.t1 / wt$wt.t2
#      profile:  /usr/bin/sample $server_pid 8             -> prof-wt$wt.sample.txt
#    All three wait for a `dig` on the ready name before loading and snapshot at
#    t+6 (then t+14); the CPU and open-loop passes also check both ports with
#    `lsof -nP -iUDP:$PORT -iUDP:$UPORT` first.
for s in thread_probe open_loop_probe profile_probe; do
  bash perf-results/macos-sample/$s.sh
done
python3 perf-results/macos-sample/analyze_probe.py \
    > perf-results/macos-sample/analysis.txt   # every table and the diagram
```

Step 7 is a read-only attribution pass: it starts nothing new on the reported ports
(15573–15578) and leaves its captures in `perf-results/macos-sample/`. `analyze_probe.py`
parses only those files — it is the single source for the numbers in "Where the ceiling
sits", so that section can be re-derived without re-measuring.

Steps 1–5 were run against `target/release/doggy-dns` while that path held the
`--features xdp` build; `perf-results/macos-driver/bin/doggy-dns-xdp-feature` is a
copy of those exact bytes (sha256 `bc02f451…`), so re-running 1–5 with `--binary`
pointed at it measures the same thing this report did.

`driver.py` is the gitignored local stand-in for `scripts/perf_ab.py`; it keeps that
runner's protocol (see the bullets above) and replaces only the three Linux reads:
`ip -d link show`, `/proc/<pid>/stat`, and `bpftool map dump`. It is deliberately not
committed: the XDP arm cannot be expressed on this platform, and a second benchmark
runner in the repo would be a maintenance cost the project already declined once.

## Limits and not covered

- **No XDP anywhere in this report** — see the earlier section. No kernel-side hit,
  no pinned map, no `bpftool`, no generic-vs-native distinction, and therefore no
  A/B ratio.
- A single host, and a laptop: 4 performance + 4 efficiency cores, no pinned
  frequency, `taskpolicy`/E-core placement decides where the tokio workers land. The
  caveat the project already publishes for its CI numbers — "Numbers are
  runner/machine specific; only same-job base-vs-PR comparisons are meaningful"
  (`docs/benchmarks/userspace-baseline.md`) — applies with more force here, where the
  comparison is across two different machines rather than across two runs of one job:
  the 132,560 QPS median on this M1 is not evidence that the userspace path doubled, it
  is evidence that one Apple core beats one Xeon SMT thread on this workload.
- Loopback only, and one arm: there is no second transport to compare against, and
  `dnsperf`, `perf-upstream` and the server share the machine, so every row carries
  the load generator's own cost (the sender-width section is what that looks like).
- Warm-cache only. Cold-cache behaviour, miss-path latency under pressure, cache
  eviction, EDNS, IPv6-only paths, DoT/DoH, the Nacos authority and the control plane
  are not measured here (the suites exercise them; the load does not).
- Every matrix row is 2 rounds (headline: 3), so a difference has to clear the
  inside-row spread quoted in each section — within ±1.2% of the row median in the
  worker-thread and sender-width rows, within ±0.3% in the `error`/`warn` rows, ±1.5% on
  the `info` row, and −2.5%/+1.9% across the three headline rounds. Nothing here is a
  gate, and the project does not treat these numbers as one:
  `docs/benchmarks/userspace-baseline.md` records that CI's `Performance` job is
  report-only ("it never fails a PR on a regression, because runner variance is not yet
  characterised well enough to gate merges on it"). A single-pass laptop run is further
  from a gate than that job is, not closer.
- Other local activity was not suppressed: Spotlight indexing (`mdworker_shared`),
  a Podman VM (`krunkit`), `coreaudiod` and this IDE were running throughout, and a
  debug `doggy-dns` instance was live on UDP 15353 (idle, 0% CPU). Where a round
  visibly overlapped a local compile, that pass was re-run and the clean pass is the
  one reported. Pass 1's headline rounds were 131,475 / 116,870 / 130,605 — round 2
  overlapped `cargo test -p doggy-dns-xdp` compiling — and pass 1 also holds a
  `worker_threads = 8` round at 87,475 that the clean pass does not reproduce
  (103,534 / 102,999). That is the size of the noise these tables are measured
  against, and the reason the readings only claim differences larger than it.
- `cargo tarpaulin` was not run: it is a Linux-first tool here and CI's coverage job
  already owns that number.
- The ceiling section is **one window per row, not two**, and its QPS values therefore
  carry no inside-row spread. They are used only for the shape: the four closed rows sit
  within 1.8% of the 2-round matrix medians, so the attribution describes a curve this
  report already measured twice over, and none of its QPS figures is a new headline.
- **Tick counts are never converted to time.** `sample` heads its own output "Sampling
  process … for 8 seconds with 1 millisecond of run time between samples", but the totals do
  not reconcile to either reading: 5,917 on-CPU ticks on a single thread over an 8 s window is
  0.74 cores, while `ps -M` measures that thread at 0.95 cores in the same configuration
  (profiler overhead is the obvious candidate, and it is not separated here). So this section
  quotes shares and counts, and never says "a `sendto` costs *n* µs". Top-of-stack entries
  also only appear "when >= 5" ticks — which is why `__recvfrom` is 455 in the histogram and
  456 in the full-depth tree — and the symbols are raw, so the "server code" row is a residual
  over un-demangled frames rather than a function-level breakdown.
- **No kernel-side attribution**: no `ktrace`, no dtrace probes, so a `sendto` tick is the
  whole syscall (socket lock, copy, and whatever the loopback driver does) as one opaque
  leaf. The 100%-under-`handle_udp` result is a statement about which *future* ran the
  syscall, not about what the kernel did inside it.
- The `-q 2000` control **does not measure a queue**. Nothing read `sysctl`/`netstat` socket
  statistics, so "the in-flight queries are held in the kernel receive queue" is an inference
  from constant QPS and 20× latency, not an observed depth; the control's own claim is only
  that 20× client concurrency leaves throughput where it was.

## Observations for follow-up (not acted on in this PR)

- The ceiling section localises the cost to code this project does not own
  (`hickory-net`'s one-datagram `UdpStream::poll_next` and its per-datagram `Vec` copy), so
  the two follow-ups it argues for are upstream or kernel-side rather than in `doggy-dns`:
  batching the syscall pair on Linux (`recvmmsg`/`sendmmsg`, UDP GSO) and trimming the
  per-request copies (`udp_stream.rs:185-188`; `handler.rs:514-520` for the `forward`/`native`
  clone path, which alloc + copy makes 15.7% of a single worker's CPU here). Neither is
  measurable on macOS, which is why this report stops at attribution.
- The same four probes would be cheap to run on the Linux CI runner (`/proc/<pid>/task/*`
  instead of `ps -M`, `perf record` instead of `sample`) to test whether the loop's `sendto`
  share is the 48.6% this host sees or something else. The perf round's Non-Goals already say
  fan-out is the change worth testing there (`docs/superpowers/specs/2026-10-29-userspace-perf-design.md:52-57`),
  and a profile of the same shape would say whether the loop is that host's binding constraint
  too.
- `worker_threads` still defaults to 1 while this host's optimum is a measured 2, and the
  ceiling section gives the reason the default is defensible for a single-core container
  but not for a multi-core one: at 1 worker the process is one thread at 0.95 cores, so the
  default is a per-core choice, not a neutral one. Changing a shipped default is the
  maintainers' call, not this report's.

- `scripts/perf_ab.py` never passes `dnsperf -c`. On 2.16.0 — this host — that leaves
  `-T` clamped to a single client; the CI image's 2.9.0 was not checked here. Either
  way the flag's stated width is not what the runner sends. Pinning `-c` explicitly
  would make it real, and the sender-width section shows the direction honestly: on
  this host one client is the *fastest* configuration, so the fix changes the load
  shape more than the headline QPS.
- A build with `--features xdp` on a non-Linux target accepts `[xdp] enabled = true`
  and says nothing at any log level. On a Linux host that cannot attach,
  `XdpManager::new` failing is loud; on a foreign OS the config is silently inert.
  A single `warn!` in `src/xdp/stub.rs::init` when `config.xdp.enabled` would close
  the gap, in the same spirit as the existing startup warnings.
- The companion Linux report says the `info` level writes "18
  `dns.request{otel.kind="server"}` span records per request", but its own published
  row does not divide that way: 866,531 lines ÷ (28,680 QPS × 10 s + 2,000 warm-up
  ≈ 288,800 requests) = **3.0** — and the ratio stays near 3 even if the warm-up
  traffic is left out of the denominator (3.02), which is exactly the per-request
  line count this host's logs reconcile to record by record. Its bytes-per-line
  (475,851,059 ÷ 866,531 = 549.1) also matches this host's 549.1. The format string
  is one shared code path, so the two hosts cannot legitimately differ in lines per
  request. This report leaves the "18" alone rather than correcting it: the
  raw log that would settle it lives on the Linux host, and re-deriving someone else's
  measurement from arithmetic here would replace their evidence with mine. Re-check it
  against that host's `perf-results`, and if it is a stale figure, fix it there.
