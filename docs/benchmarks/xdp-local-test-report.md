# XDP Fast Path — Local Test Report

- Date: 2026-11-01 (runs executed 2026-10-31, all times UTC)
- Branch under test: `perf/ab-bundle` at `904508c` (PR #28), binaries from CI
  bundle run [41227515869](https://github.com/opc-source/doggy-dns/actions/runs/41227515869)
- Nothing on this host compiles the workspace: the artifacts come from the Perf
  Bundle workflow and are checksum-verified after download.

This is a local verification record, not upstream documentation. Every number
below was produced on the machine described next; it is not a CI substitute.
The `perf-results/` and `dist/` paths referenced throughout are local run
outputs — both are gitignored, so the raw dnsperf logs, server logs and bundle
artifacts stay on that host.

## Machine specification

| Item | Value |
|---|---|
| Vendor / product | Alibaba Cloud (`Alibaba Cloud ECS`), KVM guest (`systemd-detect-virt`: `kvm`) |
| CPU | Intel(R) Xeon(R) Platinum, 2 vCPU = 1 socket × 1 core × 2 threads |
| Relevant CPU features | `avx512f avx512dq avx512bw avx512vl bmi1 bmi2 aes fma` |
| Caches | L1d 32 KiB, L1i 32 KiB, L2 1 MiB, L3 33 MiB (all single instance) |
| Memory | `MemTotal` 1,711,096 kB (1.63 GiB); ~335 MiB available at idle; **no swap** |
| Root disk | `/dev/vda3` 40 GiB virtio (21 GiB used, 17 GiB free during these runs) |
| `/tmp` | tmpfs 836 MiB — deliberately not used by any benchmark path |
| OS | Alibaba Cloud Linux 4 (`ID=alinux`) |
| Kernel | `Linux 6.6.102-8.alnx4.x86_64 #1 SMP PREEMPT_DYNAMIC Tue Sep 1 10:40:38 CST 2026 x86_64` (`uname -a`, hostname omitted) |
| glibc | 2.38 |
| Python | 3.11.6 (runs `scripts/perf_ab.py`) |
| bpftool | v7.3.0, using libbpf v1.3 |
| dnsperf (local) | 2.14.0 — the bundle manifest records 2.9.0 for the CI runner |
| BPF settings | `net.core.bpf_jit_enable = 1`, BTF present at `/sys/kernel/btf/vmlinux` |
| Limits | `ulimit -n` 524288, transparent hugepages `[always]`, no cgroup `cpu.max`/`memory.max` cap |
| XDP-capable interface | `lo` only; it has no driver, so the program attaches in **generic** mode (`ip -d link show lo` reports `xdpgeneric`) |

Practical consequence: 2 vCPU carry the server, `dnsperf` and the upstream at
the same time, and the fast path under test is generic XDP, not NIC-native XDP.

## Build under test

`scripts/fetch_perf_bundle.sh 41227515869` verified `SHA256SUMS` for
`bin/doggy-dns`, `bin/doggy-dns-xdp`, `bin/perf-upstream`, `config/*.toml`,
`perf_ab.py` and `MANIFEST.txt`. Manifest:

```
perf-upstream: static
commit: 904508c77445066eb696b6579a50f642012060a8
ref: refs/heads/perf/ab-bundle
built_at: 2026-10-31T18:09:00Z
runner: ubuntu22 Linux 6.8.0-1070-azure x86_64
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
cargo: cargo 1.99.1 (5f94df478 2026-08-27)
dnsperf: DNS Performance Testing Tool Version 2.9.0
```

What that build contains relative to `main`:

- the eBPF program no longer adds the query's claimed UDP length to a packet
  pointer (this kernel's verifier drops the scalar bound through `be16` and
  register `|`); resize safety comes from `bpf_xdp_adjust_tail` plus a
  post-resize check against the constant-bounded response length, and UDP
  payloads above 1500 bytes fall back to userspace;
- aya/aya-log 0.16 (git tag `aya-v0.16.0`) with aya-ebpf/aya-log-ebpf 0.3, and a
  loader that declares the **pinned** `DNS_CACHE` capacity when a pin already
  exists, because aya 0.16 refuses to reuse a pinned map whose `max_entries`
  differ from the declaration;
- `scripts/perf_ab.py`, `scripts/perf_upstream.c`, `scripts/fetch_perf_bundle.sh`
  and the bundle workflow.

The binaries are what every row below measured. The log-level matrix and the
`Log lines` column used the working-tree runner, which at the time carried an
uncommitted `--rust-log` option; the bundle's own copy at `904508c` still pinned
`RUST_LOG=warn`. That option is now committed as `275770f`, so the two agree
from here on.

The unit, integration and microbenchmark suites for this commit run in CI on
PR #28 (Rust: lint/build/bench/performance/coverage; XDP: format/integration/
component-benchmarks/stress — all green). They are not re-run locally here
because that would mean compiling on a 2 vCPU host.

## Userspace log-level matrix

Each row is one measured run: 1 round, `dnsperf -n 100000 -T 2 -l 10` over 1000
names, 2 server worker threads, `perf-upstream` on `127.0.0.1:15354`.
`--rust-log` sets `RUST_LOG` for the server process. `Log lines` counts what the
server wrote during the run (warm-up plus measured window).

| RUST_LOG | Arm | QPS | Avg latency (ms) | Lost | Server CPU (s) | Upstream delta | Log lines | Log size |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| error | userspace | 73,683 | 1.247 | 0 | 11.16 | 0 | 0 | 0 B |
| warn | userspace | 69,035 | 1.334 | 0 | 10.60 | 0 | 0 | 0 B |
| info | userspace | 28,680 | 3.328 | 0 | 14.36 | 0 | 866,531 | 475,851,059 B |
| warn | xdp | 218,198 | 0.015 | 0 | 0.00 | 0 | 2 | 403 B |
| info | xdp | 208,770 | 0.026 | 0 | 0.00 | 0 | 3,221 | 1,753,704 B |

Readings:

- **warn is a clean operating point.** At warn the userspace arm wrote nothing
  during its run; the XDP arm wrote only its two startup warnings (`XDP fast
  path is enabled but OTLP metrics are disabled…` and `answer rotation applies
  to userspace answers only…`). No query-path record exists at warn.
- **error and warn are indistinguishable** at this sample size: 73,683 vs
  69,035 QPS is a single round each on a 2 vCPU host, i.e. run-to-run noise.
- **info is expensive for the userspace arm.** 18
  `dns.request{otel.kind="server"}` span records per request produced 454 MiB
  in 10 s and cut throughput by 2.4× (69,035 → 28,680 QPS) while raising mean
  latency 2.5×.
- **info barely touches the XDP arm** (218,198 → 208,770 QPS, −4%): the kernel
  answers the measured traffic before userspace sees a request, so the log only
  carries the warm-up traffic.
- **The kernel-side level is not part of this matrix on purpose.** The eBPF
  program has only warn/error call sites, `AYA_LOG_LEVEL` stays unset so the
  program keeps its warn default, and no kernel record appeared during these
  runs — consistent with no abnormal packets. Kernel→userspace log delivery is
  covered by the CI integration test `xdp_kernel_logs_reach_userspace`.

## Functional matrix at `RUST_LOG=warn`

XDP binary on `127.0.0.1:15453`, `[xdp] interface = "lo"`, `cache_size = 4096`,
`negative_ttl = 2`, `pin_path = /sys/fs/bpf/doggy-e2e-warn/dns_cache`, one
`forward` plugin against a name-aware local upstream on `127.0.0.1:15454`
(A `127.0.0.9`; `nx.*` → NXDOMAIN; `ttl.*` → TTL 2; everything else TTL 60).
The upstream appends one line per answered query, so "count" below means
"queries that reached userspace forwarding".

| # | Scenario | Observed | Result |
|---|---|---|---|
| 1 | Positive miss | count 1→2, `a.warn.doggy.test. 60 IN A 127.0.0.9` | pass |
| 2 | Positive kernel hit | count stayed 2, identical answer | pass |
| 3 | Positive hit repeated | count stayed 2 | pass |
| 4 | Negative miss | count 2→3, `status: NXDOMAIN` | pass |
| 5 | Negative kernel hit | count stayed 3, `status: NXDOMAIN`, flags `qr rd` | pass |
| 6 | Short-TTL entry | upstream TTL 2 served as TTL 10 at +4 s and +9 s, count stayed 4 | pass |
| 7 | Expiry | at ~14 s the entry missed and count went 4→5 | pass |
| 8 | TCP bypass | `dig +tcp` answered, count 5→6 | pass |
| 9 | Pinned reuse across restart | stop: pin file kept, `0` `xdpgeneric` entries on `lo`; restart: `a.warn…` answered from the pinned map with count unchanged 6→6 | pass |
| 10 | Map contents | `bpftool map dump pinned …/dns_cache_v1` → 5 entries, first value starts `81 83 …` (the cached NXDOMAIN) | pass |

Things that only come out of a warn-level run:

- Row 5: the kernel-synthesised NXDOMAIN carries `RA=0`, so `dig` prints
  "WARNING: recursion requested but not available" on a kernel hit. The
  userspace answer (row 8) keeps `ra`.
- Rows 6–7: the cached TTL is not the upstream's. `compute_cache_entry` uses the
  answer's minimum TTL, and the forward plugin floors it with
  `positive_min_ttl`, whose default is `min_ttl = 10`. A 2-second answer is
  therefore kept for 10 seconds, so an expiry test has to wait past that. The
  cached TTL is also replayed verbatim (10, then 10 again) instead of counting
  down — which is exactly what the startup warning says.
- Row 9: the "reusing the pinned DNS_CACHE map; cached answers survive this
  restart" record is `INFO`. **At `RUST_LOG=warn` a successful reuse is silent**;
  only a shape or capacity mismatch warns. Confirmed by reading the complete log
  after the restart: 4 records total, all startup warnings, none traffic-linked.
  Operationally: to see provenance, run at info or check `bpftool`.

## Throughput A/B at warn

`python3 scripts/perf_ab.py --bundle dist/perf-bundle-41227515869 --rounds 2
--duration 30 --threads 2 --worker-threads 2 --queries 1000` (default
`--rust-log warn`), arm order alternating per round:

| Round | Arm | QPS | Avg latency (ms) | Lost | Server CPU (s) | Upstream delta | XDP attached | XDP hits | Log lines |
|------:|-----|----:|-----------------:|-----:|---------------:|---------------:|:------------:|---------:|----------:|
| 1 | userspace | 60,977 | 1.469 | 0 | 30.60 | 0 | no | — | 0 |
| 1 | xdp | 214,803 | 0.019 | 0 | 0.00 | 0 | yes | 6,444,082 | 2 |
| 2 | xdp | 216,662 | 0.015 | 0 | 0.00 | 0 | yes | 6,499,867 | 2 |
| 2 | userspace | 62,930 | 1.428 | 0 | 31.06 | 0 | no | — | 0 |

Medians: userspace **61,954 QPS / 1.449 ms / 30.83 CPU-s**, XDP **215,732 QPS /
0.017 ms / 0.00 CPU-s** → **3.48×** throughput with the userspace process idle
while the kernel answered. Both arms were driven by dnsperf's default **single
client**, so read this against "The load generator shares this host, on one
client" before treating it as two saturated servers.
Raw output: `perf-results/release-904508c-warn/`,
log-level runs in `perf-results/loglevel-*/`, functional matrix in
`perf-results/e2e-warn/` (including `matrix.out`, `run.sh`, `e2e_upstream.py`).

## Worker-thread matrix (`worker_threads` 1 → 2)

`server.worker_threads` is the tokio multi-thread runtime's worker count
(`src/main.rs:68`), default 1 (`crates/doggy-dns-basic/src/server.rs:29`).
`nproc` here is 2 — one physical core with two SMT threads — so 1 and 2 are the
whole range the host can address. Protocol: the same warn-level load as the
matrix above, 2 rounds × 10 s, `dnsperf -n 100000 -T 2`, 1000 names, run with
the committed `275770f` runner (`--rust-log warn`).

| worker_threads | Arm | QPS median | Rounds | Avg latency (ms) | Server CPU (s / 10 s) | Lost | Upstream delta | XDP hits | Log lines |
|---------------:|-----|-----------:|-------:|-----------------:|----------------------:|-----:|---------------:|---------:|----------:|
| 1 | userspace | 62,309 | 60,078 / 64,540 | 1.536 | 8.16 | 0 | 0 | — | 0 |
| 2 | userspace | 63,529 | 66,080 / 60,979 | 1.417 | 10.31 | 0 | 0 | — | 0 |
| 1 | xdp | 200,534 | 195,968 / 205,099 | 0.036 | 0.00 | 0 | 0 | 1,959,697 / 2,051,000 | 2 |
| 2 | xdp | 203,946 | 202,811 / 205,080 | 0.026 | 0.00 | 0 | 0 | 2,028,135 / 2,050,810 | 2 |

Reported ratios: **3.22×** at 1 worker thread, **3.21×** at 2. Both sit under the
3.48× headline because a 10 s window gives the warm-up more weight, not because
the arms changed.

Readings:

- **Doubling the workers changes neither arm's throughput beyond noise**
  (+2.0% userspace, +1.7% XDP; the round-to-round spread inside each row is
  already ±4%).
- **It costs the userspace arm 26% more CPU for nothing**: 8.16 → 10.31
  CPU-seconds per 10 s window, i.e. 0.82 → 1.03 of one core. The 30 s headline
  run at 2 workers burned 30.60 and 31.06 s, so the 10 s numbers are consistent
  rather than an artifact.
- **Why there is no gain**: `crates/doggy-dns-core/src/server.rs:20` binds a
  single UDP socket and hickory polls it in one future — `UdpStream::poll_next`
  (`hickory-net-0.26.3/src/udp/udp_stream.rs:154`) drains the reply channel with
  `poll_send_to` before it `poll_recv_from`s, so receive and reply share one task
  and a queued reply delays the next read. The per-request work in between
  (parse, plugin chain, cache lookup, serialize) does spread across workers, which
  is why the step is worth +25.4% on an 8-core host and +2.0% here. On the mac
  host the profile pins the same shape: see
  [docs/macos-local-test-report.md](macos-local-test-report.md),
  "Which task owns the socket". `worker_threads` is worth raising when the chain
  blocks on real work — a nacos lookup, TLS/DoH, a plugin doing I/O — not for
  raw UDP reply rate.
- **The XDP arm is independent of the setting**, as expected: server CPU stayed
  0.00 s in both rows while the kernel counters moved by the full completed
  count.
- Scope limit: on a 2 vCPU host that also runs `dnsperf` and the upstream, 2
  workers is already the host ceiling, so this matrix can only say "no gain up to
  `nproc`". A many-core host with a native-XDP NIC may move the userspace
  ceiling; the 8-core run in the macOS report shows exactly that (+25.4% for the
  same step).

Raw output for each value: `perf-results/worker-1/`, `perf-results/worker-2/`.

## The load generator shares this host, on one client

Every run above drives `dnsperf` **on the machine it is loading**, and
`run_dnsperf()` passes only `-s -p -d -n -T -l`: neither `-c` nor `-q`, so
dnsperf keeps its defaults of **one client** and `-q 100` outstanding queries for
that client. Verified on this host — with `-T 2` set, `ss -u -a -n -p` shows
exactly one `dnsperf` socket, because clients, not threads, own the pipeline.
So the measured window holds **at most 100 queries in flight**, and the `-T 2`
second thread adds no offered concurrency. That client, the server process and
`perf-upstream` then compete for 2 logical CPUs — one core, two SMT siblings —
plus the loopback softirq work, which generic XDP adds to the delivering CPU.

Little's Law turns the recorded QPS/latency pairs into an in-flight count, and
that is where the constraint shows:

| Run | QPS | Avg latency (ms) | In flight = QPS × latency | Of the 100 window | Server CPU |
|---|---:|---:|---:|---:|---|
| userspace, wt=1, 10 s | 62,309 | 1.536 | 95.7 | 96% | 0.82 of one core |
| userspace, wt=2, 10 s | 63,529 | 1.417 | 90.0 | 90% | 1.03 of one core |
| userspace, wt=2, 30 s headline | 61,954 | 1.449 | 89.8 | 90% | 1.03 |
| XDP, wt=1, 10 s | 200,534 | 0.036 | 7.2 | 7% | 0.00 |
| XDP, wt=2, 30 s headline | 215,732 | 0.017 | 3.7 | 4% | 0.00 |

What that says about every number in this report:

- **The userspace arm was measured closed-loop at ~90–96% of the client window.**
  With 100 queries allowed in flight and ~1.5 ms latency, QPS is pinned near
  100 / 1.5 ms ≈ 66k by the harness itself. 62,309 is therefore not the ceiling of
  `doggy-dns` on this CPU; the server's 0.82 of one core — ~18% of the window not
  runnable — is what a server that is *offered* that rate looks like.
- **The XDP arm was limited by something else entirely**: 3.7–7.2 in flight out of
  100, i.e. 4–7% of its own window. That is a single dnsperf client thread that
  cannot turn over packets fast enough — and on `lo` with generic XDP the fast path
  executes in that thread's delivery context, so the tool is paying for the answer
  it is timing.
- **The two arms are thus capped by different things**, which is the reason the
  3.48× headline must not be read as "the kernel answers 3.48× faster at
  saturation". It compares a window-capped userspace arm with a sender-CPU-capped
  kernel arm. Both absolute QPS figures are lower bounds, and these runs do not
  determine which way co-location biases the ratio.
- **The flat worker-thread curve has the same root.** 1 → 2 moved 26% more CPU
  through the server for +2.0% QPS: per-core output fell 7,636 → 6,162 QPS per
  CPU-second (−19%). With the window already ~90% full, extra workers cannot buy
  throughput; they only compete with the sender for the two SMT siblings.
- **What would settle it**: widen the client window and move the load generator
  off the measured host. The first half was done on the 8-core host in
  [docs/macos-local-test-report.md](macos-local-test-report.md) — `-T 8 -c 8`, and
  there raising `-q` from 100 (97 in flight) to 2000 (1,995 in flight) changed QPS
  by 0.3%, 104,102 → 104,409 — so once clients are wide the window stops binding
  and the server's own serial loop becomes the ceiling. No run in this report
  widened it.
- **`Server CPU 0.00` is a statement about the process, not about the machine.**
  The fast path moves reply cost out of the `doggy-dns` process; it does not remove
  it from a host that is also running the benchmark tool.

Not measured, deliberately left as an inference: `dnsperf`'s own CPU and the
whole-host utilisation. `scripts/perf_ab.py` records the server process and the
kernel counters only, so the client side above comes from its reported QPS, its
latency and its flag defaults rather than from a sample.

## Reproduce

```bash
# artifacts, no local compilation
scripts/fetch_perf_bundle.sh 41227515869      # verifies SHA256SUMS, prints the bundle path
bundle=dist/perf-bundle-41227515869

# log-level matrix, one level per invocation (error/warn/info)
sudo python3 scripts/perf_ab.py --bundle $bundle --rounds 1 --duration 10 \
  --threads 2 --worker-threads 2 --queries 1000 --only userspace --rust-log warn \
  --out perf-results/loglevel-warn-userspace
sudo python3 scripts/perf_ab.py --bundle $bundle --rounds 1 --duration 10 \
  --threads 2 --worker-threads 2 --queries 1000 --only xdp --rust-log warn \
  --out perf-results/loglevel-warn-xdp

# functional matrix at warn (root, dig, bpftool, ports 15453/15454 free)
sudo bash perf-results/e2e-warn/run.sh $bundle

# headline A/B
sudo python3 scripts/perf_ab.py --bundle $bundle --rounds 2 --duration 30 \
  --threads 2 --worker-threads 2 --queries 1000 \
  --out perf-results/release-904508c-warn

# worker-thread matrix: 1..nproc (this host: 1 and 2)
for wt in $(seq 1 "$(nproc)"); do
  sudo python3 scripts/perf_ab.py --bundle $bundle --rounds 2 --duration 10 \
    --threads 2 --worker-threads "$wt" --queries 1000 --rust-log warn \
    --out "perf-results/worker-$wt"
done
```

`run.sh` drives `e2e_upstream.py`, the name-aware upstream the functional matrix
needs: the bundled `perf-upstream` answers every A query positively, so it
cannot produce the NXDOMAIN or short-TTL cases.

## Limits and not covered

- `lo` attaches in **generic** mode; these numbers say nothing about native NIC
  XDP throughput. `dnsperf` also runs on this host, driven by its default
  **one client / `-q 100`** window, so every QPS below is a lower bound and the
  userspace arm is partly a harness number — see "The load generator shares this
  host, on one client".
- No client-side or whole-host CPU was sampled; the runner records the server
  process and the kernel counters only.
- The runner exposes no `--clients`/`--outstanding` knob; widening the client
  window needs a `dnsperf` flag change (`run_dnsperf()`), which is the first
  thing to add before these numbers are compared against the macOS run.
- Both arms are warm-cache. Cold-cache behaviour, miss-path latency under
  pressure and LRU eviction are not measured here.
- No IPv6, no fragment reassembly, no EDNS `DO`/large-payload handling, no
  DoT/DoH, no multi-queue, no concurrent senders.
- Kernel counters are not exported (OTLP disabled in these configs); kernel
  hits were proven with a flat upstream counter and `bpftool` dumps of
  `XDP_STATS`, which the runner does for the XDP arm.
- The log-level matrix covers userspace levels only, one round per row, so
  differences under ~10% are noise. Kernel error/off variants are not measured:
  the program's warn/error sites fire only on abnormal packets, which this load
  does not produce.
- The worker-thread matrix cannot exceed `nproc` = 2 on this host, and 2 is
  already the ceiling shared with `dnsperf` and the upstream, so it measures
  "no gain up to the host's CPU count", not scaling behaviour beyond it.
- Local toolchain note, kept because it explains the CI-build approach:
  `/usr/local/bin/rustc` is 1.88 while `Cargo.lock` needs ≥1.89, so any local
  compile must put `/root/.cargo/bin` first on `PATH`.

## Appendix — earlier local-build session (superseded)

Before the bundle workflow existed, the same suite was run from a local build of
`ad1d6ae` plus the interim `read_u16_be` length-composition workaround (an
approach later replaced by removing the resize-time pointer check). Recorded
here so the numbers are not mistaken for results of the current branch:

- `cargo test -p doggy-dns-xdp`: 39 passed, 0 failed
  (`cache_test` 10, `ebpf_artifact_test` 1, `interface_test` 4,
  `kernel_log_test` 6, `middleware_test` 4, `pin_test` 7, `stats_test` 7).
- Root integration suite: 9 passed, 0 failed in 4.02 s with XDP on `lo`,
  including `xdp_pinned_cache_survives_a_restart` and
  `xdp_reuse_warns_when_the_pinned_map_does_not_fit` (both predated the aya 0.16
  pinned-capacity fix).
- `cargo bench -p doggy-dns-xdp` (release + LTO): `fnv1a_64_short` 10.30 ns,
  `fnv1a_64_long` (255 B) 26.35 ns, `make_cache_key` 44.91 ns,
  `cache_value_positive` 50.16 ns, `cache_value_negative` 59.29 ns.
- The e2e matrix then ran at the default log level and reported the reuse proof
  from the `INFO` line, which is why the warn-level rerun above reads the pin
  with `bpftool` instead.
