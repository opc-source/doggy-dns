# Doggy-DNS

狗狗 DNS, 可以 DaemonSet 或中心化权威部署？暂时不完善，但作为本地代理完全可用。想试试 AI Coding 有多强，是真的强一天做了以前作者应该需要一两个月的事，主要是作者水平不够而它能拔高了所有基础能力，基本所有代码都是 AI 生成的！！！

A Rust-based service discovery DNS server built on [hickory-dns](https://github.com/hickory-dns/hickory-dns), inspired by CoreDNS's plugin architecture. Resolves service names from registries like Nacos to IP addresses via standard DNS queries.

## Architecture

```
                          DNS Query (UDP/TCP/TLS/HTTPS)
                                      |
                                      v
                           +--------------------+
                           |  DoggyDnsHandler   |
                           +--------------------+
                                      |
                    +-----------------+-----------------+
                    |                                   |
                    v                                   v
          Middleware Chain                     Authority Chain
        (before / after)                   (sequential lookup)
                    |                                   |
          +---------+---------+           +-------------+-------------+
          |                   |           |             |             |
          v                   v           v             v             v
      Logging            Metrics      Nacos       Native       Forward
    Middleware          Middleware   Authority     Authority      Authority
```

### Crate Structure

```
doggy-dns/                    # Binary entrypoint
  crates/
    doggy-dns-plugin/         # Plugin traits, authority chain, built-in authorities
    doggy-dns-core/           # Server runtime, config, handler, middleware
    doggy-dns-nacos/          # Nacos service discovery backend
```

**Dependency graph:**

- `doggy-dns` (binary) -> `doggy-dns-core`, `doggy-dns-plugin`, `doggy-dns-nacos`
- `doggy-dns-core` -> `doggy-dns-plugin`
- `doggy-dns-nacos` -> `doggy-dns-plugin`
- `doggy-dns-plugin` is the leaf crate (no internal deps)

### Request Flow

1. **Middleware chain** runs `before_request()` on each middleware in order. If any returns `ShortCircuit`, the request is refused immediately.
2. **Authority chain** iterates handlers sequentially. Each handler returns one of:
   - `Skip` -- does not handle this query, try the next handler
   - `Continue(Ok(records))` -- resolved, return records
   - `Continue(Err(e))` / `Break(Err(e))` -- error, return SERVFAIL
3. If all handlers skip, the response is `NXDOMAIN`.
4. Middleware chain runs `after_request()` with elapsed time.

### Plugin Native

Two plugin axes:

| Layer | Trait | Purpose | Implementations |
|-------|-------|---------|-----------------|
| Middleware | `Middleware` | Cross-cutting concerns | `LoggingMiddleware`, `MetricsMiddleware` |
| Authority | `ZoneHandler` | DNS data sources | `NacosAuthority`, `NativeAuthority`, `ForwardAuthority` |

**Authority plugins:**

| Kind | Description |
|------|-------------|
| `nacos` | Resolves service names from Nacos registry via DashMap cache. Format: `<service>.<group>.<namespace>.<zone>.` |
| `native` | Resolves using the OS system resolver (`/etc/resolv.conf`) |
| `forward` | Resolves using explicit upstream DNS servers (e.g., `8.8.8.8:53`) |

Plugins are evaluated in the order defined in the `[[plugins]]` config array. The first handler that returns a non-Skip result wins.

### Nacos Integration

- Connects to Nacos naming service via `nacos-sdk`
- Subscribes to all services on startup; receives push updates on instance changes
- Maintains an in-memory `DashMap<ServiceKey, Vec<CachedInstance>>` cache
- DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.`
- Example: `dig user-service.DEFAULT_GROUP.public.nacos.local A`
- Only handles `A` and `AAAA` record types; all other types are skipped

### Server Protocols

| Protocol | Default Port | Description |
|----------|-------------|-------------|
| UDP | config `port` | Standard DNS |
| TCP | config `port` | Standard DNS over TCP |
| TLS (DoT) | 853 | DNS over TLS, requires `[server.tls]` |
| HTTPS (DoH) | 443 | DNS over HTTPS at `/dns-query`, requires `[server.https]` |

### Logging

Structured logging via `tracing`. Key log events:

| Event | Level | Fields |
|-------|-------|--------|
| Query resolved | `INFO` | `query`, `qtype`, `authority`, `answers`, `duration_ms`, `response_code` |
| No authority handled query | `INFO` | `query`, `qtype`, `response_code=NXDomain` |
| Authority returned error | `WARN` | `query`, `qtype`, `authority`, `error`, `response_code=ServFail` |
| Middleware short-circuit | `WARN` | `duration_ms`, `response_code=Refused` |
| Configuration loaded | `INFO` | `listen_addr`, `port`, `worker_threads`, `plugin_count`, ... |

Control log level via the `RUST_LOG` environment variable.

## Configuration

TOML-based. Default path: `config/doggy-dns.toml` (or pass as CLI arg).

```toml
[server]
listen_addr = "0.0.0.0"
port = 15353
tcp_timeout = 10          # seconds, default 10
shutdown_timeout = 5      # seconds, default 5
worker_threads = 1        # default 1 (suitable for DaemonSet, one per node)

# Optional: DNS over TLS
#[server.tls]
#enabled = true
#port = 853
#cert_path = "/path/to/cert.pem"
#key_path = "/path/to/key.pem"

# Optional: DNS over HTTPS
#[server.https]
#enabled = true
#port = 443
#cert_path = "/path/to/cert.pem"
#key_path = "/path/to/key.pem"

[middleware]
logging = true
metrics = false

# Plugins are evaluated in order. First non-Skip result wins.
[[plugins]]
kind = "nacos"
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "native"
enabled = true
cache_size = 4096         # default 4096
min_ttl = 10              # seconds, default 10
max_ttl = 300             # seconds, default 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53", "1.1.1.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600

# Optional: hot-reload config from Nacos config service
[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DOGGY_DNS"
data_id = "doggy-dns.toml"
```

If no `[[plugins]]` are configured, defaults to forwarding to `8.8.8.8` and `1.1.1.1`.

## Quick Start

### Build

```bash
cargo build --release
```

### Run

```bash
# With default config
./target/release/doggy-dns config/doggy-dns.toml

# With debug logging
RUST_LOG=doggy_dns=debug ./target/release/doggy-dns config/doggy-dns.toml
```

### Test

```bash
# Run all tests
cargo test --workspace

# Query the server (once running)
dig @127.0.0.1 -p 15353 www.google.com A
dig @127.0.0.1 -p 15353 user-service.DEFAULT_GROUP.public.nacos.local A
```

### Benchmarks

Benchmarks use [criterion](https://crates.io/crates/criterion) and parameterize over `worker_threads` 1..4.

```bash
# Run all benchmarks (nacos, authority chain, handler pipeline)
cargo bench --workspace

# Run a single crate's benchmarks
cargo bench -p doggy-dns-core       # handler pipeline benchmarks
cargo bench -p doggy-dns-nacos      # nacos authority lookup benchmarks
cargo bench -p doggy-dns-plugin     # authority chain benchmarks

# Run a single benchmark by name
cargo bench --bench handler_bench
cargo bench --bench nacos_bench
cargo bench --bench chain_bench

# Filter to a specific benchmark function
cargo bench --bench handler_bench -- handler_pipeline_nacos_hit

# Compile-only check (used in CI)
cargo bench --no-run --workspace
```

> **Note:** `cargo bench` output starts with "running 0 tests" lines from the default test harness for each crate. This is normal -- the actual criterion results follow immediately after.

### Coverage

Uses [cargo-tarpaulin](https://github.com/xd009642/tarpaulin) with config in `tarpaulin.toml`.

```bash
# Install tarpaulin
cargo install cargo-tarpaulin

# Run coverage with project config (outputs XML + HTML reports)
cargo tarpaulin --config tarpaulin.toml

# Quick terminal summary
cargo tarpaulin --workspace --out Stdout

# Generate HTML report only
cargo tarpaulin --workspace --out Html
# Open tarpaulin-report.html in browser
```

Reports: `cobertura.xml` (for CI/Codecov) and `tarpaulin-report.html` (for local viewing).

## Development Rules

### Coding Standards

- **Clippy**: `cargo clippy -- -D warnings` (all warnings are errors in CI)
- **Format**: `cargo fmt --check` enforced in CI
- **Edition**: Rust 2024, stable toolchain
- **Max line width**: 100 characters (rustfmt)

### Disallowed APIs (`.clippy.toml`)

| Disallowed | Use Instead | Reason |
|------------|-------------|--------|
| `std::env::set_var` / `remove_var` | -- | Data race in multi-threaded context |
| `std::thread::sleep` | `tokio::time::sleep` | Blocks the async runtime |
| `.unwrap()` | `.expect("reason")` or `?` | Require descriptive error context |
| `std::time::Instant` | `tokio::time::Instant` | Consistent with tokio runtime |

`.unwrap()` is allowed in test files via `#![allow(clippy::disallowed_methods)]`.

### Error Handling

- Use `anyhow::Result` with `.context()` for descriptive errors
- Propagate errors with `?`, never silently swallow
- Validate config at startup (cert paths exist, worker_threads > 0, etc.)

### Immutability

- Cache updates replace the entire `Vec` for a key (never mutate in place)
- Config structs are `Clone` -- new configs are built, not mutated
- `Arc<DashMap>` for concurrent read-heavy access

### CI Pipeline

| Job | What it does |
|-----|-------------|
| **Lint** | `cargo fmt --check` + `cargo clippy -- -D warnings` |
| **Build & Test** | `cargo build` + `cargo test` |
| **Benchmarks** | `cargo bench --no-run` (compile check only) |
| **Coverage** | `cargo-tarpaulin` -> Codecov (target: 60% project, 70% patch) |

All jobs use dependency caching (`Swatinem/rust-cache@v2`).

### Test Structure

| Test File | Tests | Description |
|-----------|-------|-------------|
| `crates/doggy-dns-core/tests/config_test.rs` | 13 | Config parsing, defaults, validation (TLS/HTTPS/remote_config) |
| `crates/doggy-dns-core/tests/handler_test.rs` | 7 | Handler pipeline: NoError, NXDomain, ServFail, Refused, middleware callbacks |
| `crates/doggy-dns-core/tests/middleware_test.rs` | 6 | Metrics counter, logging middleware, return values |
| `crates/doggy-dns-nacos/tests/authority_test.rs` | 3 | Nacos authority lookup hit/miss/wrong-zone |
| `crates/doggy-dns-nacos/tests/mapping_test.rs` | 6 | DNS name parsing, record creation |
| `crates/doggy-dns-nacos/tests/watcher_test.rs` | 6 | Instance cache: insert, filter, remove, replace, lowercase keys |
| `crates/doggy-dns-plugin/tests/authority_chain_test.rs` | 9 | Chain resolution: empty, hit, miss, fallthrough, all-skip, Break, error, is_empty |
| `crates/doggy-dns-plugin/tests/forward_test.rs` | 1 | Forward resolver (requires network) |
| `crates/doggy-dns-plugin/tests/native_test.rs` | 1 | Native resolver (requires network) |
| `tests/integration_test.rs` | 2 | End-to-end chain: Nacos hit, Nacos miss -> Forward |

### Benchmark Structure

| Bench File | Benchmarks | Description |
|------------|------------|-------------|
| `crates/doggy-dns-core/benches/handler_bench.rs` | 2 x 4 | Handler pipeline (nacos hit, nxdomain) x worker_threads 1..4 |
| `crates/doggy-dns-nacos/benches/nacos_bench.rs` | 2 x 4 + 3 | Authority lookup (hit, miss) x worker_threads 1..4 + sync benchmarks |
| `crates/doggy-dns-plugin/benches/chain_bench.rs` | 2 x 4 | Authority chain (empty, nacos hit) x worker_threads 1..4 |

### Key Dependencies

| Library | Purpose |
|---------|---------|
| [hickory-dns](https://github.com/hickory-dns/hickory-dns) | DNS server framework + resolver + protocol types |
| [nacos-sdk](https://crates.io/crates/nacos-sdk) | Nacos service discovery and config client |
| [tokio](https://tokio.rs) | Async runtime |
| [dashmap](https://crates.io/crates/dashmap) | Concurrent hash map for service instance cache |
| [rustls](https://crates.io/crates/rustls) | TLS implementation for DoT/DoH |
| [tracing](https://crates.io/crates/tracing) | Structured logging |
| [criterion](https://crates.io/crates/criterion) | Benchmarking |

## Roadmap

- [ ] Kubernetes service discovery (`kube-rs` / `k8s-openapi`)
- [ ] etcd backend (`etcd-rs`)
- [ ] Redis backend (`redis-rs`)
- [ ] Database backend (`sqlx`)
- [ ] Dynamic plugin registry (load plugins at runtime)
- [ ] Config hot-reload via Nacos config service (watcher implemented, wiring pending)
- [ ] Metrics histogram for query latencies

## License

[Apache License Version 2.0](LICENSE)

## Acknowledgement

- Inspired by DNS-F from Alibaba and [CoreDNS](https://coredns.io)
- Built on [hickory-dns](https://github.com/hickory-dns/hickory-dns)
