# Development Guide

Part of [README](../README.md). The same commands and rules are mirrored for
AI agents in [AGENTS.md](../AGENTS.md) — when conventions change, update both.

## Build, test, bench, coverage

```bash
cargo build                                    # Build all crates
cargo test --workspace                         # Run all tests (some need network)
cargo test -p doggy-dns-nacos                 # Test a single crate
cargo test -p doggy-dns-plugin --test authority_chain_test  # Single test file
cargo test -p doggy-dns-basic --test config_test -- parse_full_config  # Single test
cargo clippy --all-features --all-targets -- -D warnings    # Lint (CI uses -D, not -W)
cargo fmt --all -- --check                     # Format check
cargo bench --no-run --workspace               # Verify benchmarks compile
cargo bench --workspace                        # Run all benchmarks
cargo bench -p doggy-dns-core                 # Handler pipeline benchmarks only
cargo bench --bench nacos_bench                # Single benchmark file
cargo bench --bench handler_bench -- handler_pipeline_nacos_hit  # Filter by name
cargo tarpaulin --config tarpaulin.toml        # Coverage (XML + HTML reports)
cargo tarpaulin --workspace --out Stdout       # Coverage quick summary

# Nacos end-to-end (requires a standalone Nacos server; see "CI pipeline")
DOGGY_NACOS_ADDR=127.0.0.1:8848 cargo test --test nacos_e2e_test -- --ignored --nocapture

# XDP (optional feature; requires Linux + bpf-linker; see xdp.md for usage)
cargo build --features xdp                     # Root binary with XDP support
cd xdp && cargo build --target bpfel-unknown-none --release  # eBPF program
cargo test -p doggy-dns-xdp                    # XDP userspace crate tests
cargo bench -p doggy-dns-xdp                   # XDP components
sudo env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored  # Integration (root)
```

> **Note:** `cargo bench` output starts with "running 0 tests" lines from the
> default test harness for each crate. This is normal — the actual criterion
> results follow immediately after. Benchmarks inherit `[profile.release]`
> (`lto = "fat"`, `codegen-units = 1`), so a run is slower to build and its
> numbers are not comparable with runs recorded before that profile existed.

> The `Performance` job runs the component benches twice in one job (main first,
> PR second) and prints criterion `change:` lines, plus one end-to-end dnsperf run
> against `config/doggy-dns-stress.toml` with `RUST_LOG=warn`. It never fails a PR:
> shared-runner variance is not characterised well enough to gate merges. The
> comparison is lenient, so a bench ID that is new or renamed on the PR side is
> measured but has no `change:` line. The end-to-end Nacos scenario is absent by
> necessity — the server aborts boot when Nacos is unreachable — so the Nacos
> paths are covered by benches only. `docs/benchmarks/userspace-baseline.md`
> holds the reference numbers, and `docs/benchmarks/xdp-kernel-hot-path.md`
> holds the XDP program's instruction counts and component benchmarks.

> Both end-to-end arms — here and in the `XDP Stress Test` job — are limited by
> dnsperf's `-l` wall clock (30 s), not by `-n`: `-n` only cycles the input
> file, and 10 cycles of 1000 names finish in well under a second, which
> measured warm-up rather than steady state. The stress job reports XDP against
> userspace from one run, not PR against main, so a ratio that moves a few
> percent says nothing about a kernel change; the component benches and the
> instruction counts are the kernel signals.

Reports: `cobertura.xml` (for CI/Codecov) and `tarpaulin-report.html` (local).

## Coding standards

- **Clippy**: `cargo clippy -- -D warnings` (all warnings are errors in CI)
- **Format**: `cargo fmt --check` enforced in CI; `rustfmt.toml`: edition 2024, max width 100
- **Edition**: Rust 2024, stable toolchain

### Disallowed APIs (`.clippy.toml`)

| Disallowed | Use Instead | Reason |
|------------|-------------|--------|
| `std::env::set_var` / `remove_var` | -- | Data race in multi-threaded context |
| `std::thread::sleep` | `tokio::time::sleep` | Blocks the async runtime |
| `.unwrap()` | `.expect("reason")` or `?` | Require descriptive error context |
| `std::time::Instant` | `tokio::time::Instant` | Consistent with tokio runtime |

`.unwrap()` is allowed in test files via `#![allow(clippy::disallowed_methods)]`.

### Error handling

- Use `anyhow::Result` with `.context()` for descriptive errors
- Propagate errors with `?`, never silently swallow
- Validate config at startup (see [configuration.md](configuration.md))

### Immutability

- Cache updates replace the entire `Vec` for a key (never mutate in place)
- Config structs are `Clone` — new configs are built, not mutated
- `Arc<DashMap>` for concurrent read-heavy access

## CI pipeline

| Job | What it does |
|-----|-------------|
| **Lint** | `cargo fmt --check` + `cargo clippy -- -D warnings` |
| **Build & Test** | `cargo build` + `cargo test` |
| **Benchmarks** | `cargo bench --no-run` (compile check) + `Performance` job (component benches base-vs-PR, one end-to-end dnsperf run) |
| **Coverage** | `cargo-tarpaulin` -> Codecov (target: 60% project, 70% patch) |
| **XDP CI** | fmt, integration tests (`--ignored`, root), component benches vs. base, dnsperf stress |
| **Nacos CI** | standalone Nacos server + `--ignored` end-to-end tests (subscribe → push → DNS answer) |
| **Image** | builds both container variants on native amd64/arm64 runners, smoke-tests each, then publishes multi-arch manifests to ghcr.io |

Every job that compiles Rust caches the dependency build
(`Swatinem/rust-cache@v2`); the format-only checks do not need it, and the Image
job cannot use it because it compiles inside a container.

The Image job is the only workflow that produces artifacts rather than verdicts,
so it is a push trigger on `main` alone. [deployment.md](deployment.md) covers the
Dockerfile's build arguments, the `CARGO_EXTRA_ARGS` memory workaround for small
build machines, and how to run the same smoke locally:

```bash
python3 scripts/image_smoke.py --image <tag> --runtime podman          # userspace
python3 scripts/image_smoke.py --image <tag> --runtime podman --xdp    # + pinned-map reuse
python3 -m unittest scripts.test_image_smoke                          # the script's own tests
```

### Running the Nacos end-to-end tests locally

The cases in `tests/nacos_e2e_test.rs` drive a real `doggy-dns` process against
a live Nacos server, so they are `#[ignore]`d and only run when asked for:

```bash
podman run -d --name doggy-nacos \
  -e MODE=standalone -e JVM_XMS=384m -e JVM_XMX=512m \
  -p 8848:8848 -p 9848:9848 \
  docker.m.daocloud.io/nacos/nacos-server:v2.5.4
DOGGY_NACOS_ADDR=127.0.0.1:8848 \
  cargo test --test nacos_e2e_test -- --ignored --nocapture
```

Points worth knowing before reading a failure:

- **Subscriptions are established once, at start-up** (`subscribe_all()` in
  `src/main.rs`); there is no periodic rescan and no subscribe-on-miss, so a
  service created in Nacos after doggy-dns started is only picked up on restart.
- **Ephemeral instances live and die with the registering client's connection.**
  The test keeps its `NamingService` handle alive for exactly this reason.
- **A batch registration replaces the service's instance list** rather than
  appending to it, so adding an instance means re-registering the whole list.
- `namespaceId=public` and the empty namespace are the same namespace on the
  server, so the config's `namespace = "public"` needs no special handling.

## Test structure

| Test File | Tests | Description |
|-----------|-------|-------------|
| `crates/doggy-dns-basic/tests/config_test.rs` | 54 | Config parsing, defaults, validation (plugins/TLS/HTTPS/remote_config/observability/xdp/control plane) |
| `crates/doggy-dns-basic/tests/env_test.rs` | 8 | `${ENV_VAR}` expansion across tables and arrays, malformed references |
| `crates/doggy-dns-basic/tests/net_test.rs` | 5 | IPv4/IPv6 listen-address parsing, bracketed literals, rejects hostnames |
| `crates/doggy-dns-core/tests/handler_test.rs` | 17 | Handler pipeline: NoError, NXDomain, NODATA, ServFail, Refused, middleware callbacks, EDNS cache admission, AA/RA flags, SOA on negatives |
| `crates/doggy-dns-core/tests/middleware_test.rs` | 2 | Logging middleware: `before_request` continues, `after_request` completes |
| `crates/doggy-dns-core/tests/server_test.rs` | 2 | UDP/TCP listener binding for bare and bracketed IPv6 addresses |
| `crates/doggy-dns-core/tests/wire_golden_test.rs` | 5 | Golden wire bytes for the Nacos paths: hit (name lowercased), NXDOMAIN, NODATA, apex SOA |
| `crates/doggy-dns-nacos/tests/alloc_guard_test.rs` | 1 | Allocation budget for `NacosAuthority::lookup`: 6 on a hit, 2 on an in-zone miss |
| `crates/doggy-dns-nacos/tests/authority_test.rs` | 15 | Nacos authority: hit, dotted names, NOERROR/NXDOMAIN/NODATA, family filtering, zone boundaries, apex SOA |
| `crates/doggy-dns-nacos/tests/mapping_test.rs` | 11 | Name-to-key mapping (zone boundaries, dotted services, escaped labels), family-filtered record creation |
| `crates/doggy-dns-nacos/tests/watcher_test.rs` | 11 | Instance cache: insert, filter, remove, replace, lowercase keys, push-freshness stamp, address parsing at push |
| `crates/doggy-dns-plugin/tests/authority_chain_test.rs` | 12 | Chain resolution: empty, hit, authoritative miss, zone fallthrough, all-skip, Break, error, is_empty |
| `crates/doggy-dns-plugin/tests/forward_test.rs` | 9 | Forward resolver: local mock upstream, custom/bare IP parsing, validation, NXDOMAIN/NODATA/SERVFAIL mapping, underscore labels still forwarded |
| `crates/doggy-dns-plugin/tests/request_outcome_test.rs` | 3 | `RequestOutcome` construction: success, response bytes, NXDOMAIN TTL |
| `crates/doggy-dns-plugin/tests/system_dns_test.rs` | 1 | Native resolver (requires network) |
| `crates/doggy-dns-plugin/tests/wire_query_name_test.rs` | 2 | The wire-name handoff both fallback authorities use: label bytes survive, and the `&str` path's underscore rejection stays a premise |
| `crates/doggy-dns-xdp/tests/*` | 22 + 6 ignored | Userspace cache/interface/middleware/stats; XDP attach tests need root + Linux |
| `crates/doggy-dns-control/tests/*` | 29 | Control plane: HTTP contract, endpoint selection, token scoping, live signals, Prometheus `/metrics`, state snapshots, redaction |
| `tests/integration_test.rs` | 3 | End-to-end chain: Nacos hit, outside-zone fallthrough, in-zone miss stays authoritative |
| `tests/nacos_e2e_test.rs` | 4 ignored | Live Nacos: boot-time instances, push after start-up, in-zone NXDOMAIN, IPv4-only NODATA |

Forward tests run against a local mock upstream, native tests make real
network calls, and the Nacos unit tests use mock DashMap caches (no Nacos
server needed); only `tests/nacos_e2e_test.rs` requires a running server, which
`.github/workflows/nacos-ci.yml` provides.

## Benchmark structure

Benchmarks use [criterion](https://crates.io/crates/criterion) and
parameterize over `worker_threads` 1..4.

| Bench File | Benchmarks | Description |
|------------|------------|-------------|
| `crates/doggy-dns-core/benches/handler_bench.rs` | 3 x 4 | Handler pipeline (nacos hit, nacos nxdomain, empty-chain nxdomain) x worker_threads 1..4 |
| `crates/doggy-dns-nacos/benches/nacos_bench.rs` | 2 x 4 + 4 | Authority lookup (hit, miss) x worker_threads 1..4 + service-key hit/miss/dotted and `to_dns_records` |
| `crates/doggy-dns-plugin/benches/chain_bench.rs` | 2 x 4 | Authority chain (empty, nacos hit) x worker_threads 1..4 |

## Key dependencies

| Library | Purpose |
|---------|---------|
| [hickory-dns](https://github.com/hickory-dns/hickory-dns) | DNS server framework + resolver + protocol types |
| [nacos-sdk](https://crates.io/crates/nacos-sdk) | Nacos service discovery and config client |
| [tokio](https://tokio.rs) | Async runtime |
| [dashmap](https://crates.io/crates/dashmap) | Concurrent hash map for service instance cache |
| [rustls](https://crates.io/crates/rustls) | TLS implementation for DoT/DoH |
| [tracing](https://crates.io/crates/tracing) | Structured logging |
| [opentelemetry](https://crates.io/crates/opentelemetry) | OTLP logs, metrics, traces and profiling resource model |
| [pprof](https://crates.io/crates/pprof) | In-process CPU profiling for OTLP profiles |
| [criterion](https://crates.io/crates/criterion) | Benchmarking |
| [aya](https://aya-rs.dev) | Optional XDP fast path (`--features xdp`, Linux only) |
