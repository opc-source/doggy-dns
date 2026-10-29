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
> end-to-end Nacos scenario is absent by necessity — the server aborts boot when
> Nacos is unreachable — so the Nacos paths are covered by benches only.
> `docs/benchmarks/userspace-baseline.md` holds the reference numbers.

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

All jobs use dependency caching (`Swatinem/rust-cache@v2`).

## Test structure

| Test File | Tests | Description |
|-----------|-------|-------------|
| `crates/doggy-dns-basic/tests/config_test.rs` | 49 | Config parsing, defaults, validation (plugins/TLS/HTTPS/remote_config/observability/xdp/control plane) |
| `crates/doggy-dns-basic/tests/env_test.rs` | 8 | `${ENV_VAR}` expansion across tables and arrays, malformed references |
| `crates/doggy-dns-basic/tests/net_test.rs` | 5 | IPv4/IPv6 listen-address parsing, bracketed literals, rejects hostnames |
| `crates/doggy-dns-core/tests/handler_test.rs` | 17 | Handler pipeline: NoError, NXDomain, NODATA, ServFail, Refused, middleware callbacks, EDNS cache admission, AA/RA flags, SOA on negatives |
| `crates/doggy-dns-core/tests/middleware_test.rs` | 6 | Metrics counter, logging middleware, return values |
| `crates/doggy-dns-core/tests/server_test.rs` | 2 | UDP/TCP listener binding for bare and bracketed IPv6 addresses |
| `crates/doggy-dns-nacos/tests/authority_test.rs` | 12 | Nacos authority: hit, dotted names, NOERROR/NXDOMAIN/NODATA, family filtering, zone boundaries |
| `crates/doggy-dns-nacos/tests/mapping_test.rs` | 10 | DNS name parsing (zone boundaries, dotted services), family-filtered record creation |
| `crates/doggy-dns-nacos/tests/watcher_test.rs` | 7 | Instance cache: insert, filter, remove, replace, lowercase keys, push-freshness stamp |
| `crates/doggy-dns-plugin/tests/authority_chain_test.rs` | 11 | Chain resolution: empty, hit, authoritative miss, zone fallthrough, all-skip, Break, error, is_empty |
| `crates/doggy-dns-plugin/tests/forward_test.rs` | 8 | Forward resolver: local mock upstream, custom/bare IP parsing, validation, NXDOMAIN/NODATA/SERVFAIL mapping |
| `crates/doggy-dns-plugin/tests/native_test.rs` | 1 | Native resolver (requires network) |
| `crates/doggy-dns-xdp/tests/*` | 22 + 6 ignored | Userspace cache/interface/middleware/stats; XDP attach tests need root + Linux |
| `crates/doggy-dns-control/tests/*` | 28 | Control plane: HTTP contract, endpoint selection, token scoping, live signals, state snapshots, redaction |
| `tests/integration_test.rs` | 3 | End-to-end chain: Nacos hit, outside-zone fallthrough, in-zone miss stays authoritative |

Forward tests run against a local mock upstream, native tests make real
network calls, and Nacos tests use mock DashMap caches (no Nacos server
needed).

## Benchmark structure

Benchmarks use [criterion](https://crates.io/crates/criterion) and
parameterize over `worker_threads` 1..4.

| Bench File | Benchmarks | Description |
|------------|------------|-------------|
| `crates/doggy-dns-core/benches/handler_bench.rs` | 2 x 4 | Handler pipeline (nacos hit, nxdomain) x worker_threads 1..4 |
| `crates/doggy-dns-nacos/benches/nacos_bench.rs` | 2 x 4 + 3 | Authority lookup (hit, miss) x worker_threads 1..4 + sync benchmarks |
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
