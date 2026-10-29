# Userspace Performance Baseline

Two kinds of number live here, and they are not interchangeable:

- **Component benchmarks** are criterion medians measured locally on the machine
  named below, from the committed benches. They are the starting point later PRs
  compare against.
- **End-to-end** is measured only by the `Performance` CI job, whose base-vs-PR
  comparison happens on one runner inside one job. That job's first run lands
  with this PR, so the end-to-end row below is the reference from the XDP stress
  job's historical userspace-only run, not a measurement taken by this job.

Numbers are runner/machine specific; only same-job base-vs-PR comparisons are
meaningful.

- Date: 2026-10-29
- CI target: `ubuntu-latest` (2 vCPU shared)
- Local bench machine: CherishCai, Apple M1, 8 cores, macOS 26.6.2 (Darwin
  arm64), rustc 1.99.1 (stable)
- Binary: `cargo build --release`, feature `xdp` off
- Server config: `config/doggy-dns-stress.toml` (forward to 8.8.8.8:53,
  `worker_threads = 4`), `RUST_LOG=warn`
- Load: `dnsperf -s 127.0.0.1 -p 5353 -d /tmp/queries.txt -n 10 -T 4 -l 30`
  over 1000 names, after one 5 s warm-up pass

## End-to-end

| Metric | Value |
|--------|-------|
| Queries per second | not measured by this job yet |
| Queries lost | not measured by this job yet |
| Average latency (s) | not measured by this job yet |

Reference: **43,288 QPS** with four worker threads, from the XDP stress job's
userspace-only run recorded in `docs/superpowers/specs/2026-10-29-userspace-perf-design.md`.
The first `Performance` job on this PR replaces the table above with its own
number; if it is not the same order of magnitude, the job is measuring something
other than the transport (check the `RUST_LOG=warn` suppression and the warm-up).

## Component benchmarks (median per iteration, measured locally)

| Benchmark | threads=1 | threads=4 |
|-----------|-----------|-----------|
| `handler_pipeline_nacos_hit` | 2.3416 µs | 2.2664 µs |
| `handler_pipeline_nacos_nxdomain` | 2.9053 µs | 2.9503 µs |
| `handler_pipeline_nxdomain` | 550.41 ns | 555.58 ns |
| `authority_chain_nacos_hit` | 1.4863 µs | 1.4787 µs |
| `nacos_authority_lookup_hit` | 1.4314 µs | 1.4131 µs |

## Reproduce

```bash
cargo build --release
RUST_LOG=warn ./target/release/doggy-dns config/doggy-dns-stress.toml &
dnsperf -s 127.0.0.1 -p 5353 -d /tmp/queries.txt -n 10 -T 4 -l 30
cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline base
```

## Known gaps

- The Nacos **hit** path is covered only by criterion (`handler_bench`,
  `chain_bench`, `nacos_bench`), never end-to-end: `NacosServiceWatcher::new`
  propagates a connection error and aborts boot, so a Nacos stress config
  requires a live server in the job.
- The `forward` scenario's answer set is whatever `8.8.8.8` returns, so its
  QPS is partly the upstream's. It measures our transport, not our compute.
- The job is report-only: it never fails a PR on a regression, because runner
  variance is not yet characterised well enough to gate merges on it.
