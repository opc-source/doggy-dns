# AGENTS.md

This file provides guidance to AI Coding when working with code in this repository.

Human-facing documentation lives in `README.md` and `docs/` (`architecture.md`,
`configuration.md`, `observability.md`, `xdp.md`, `development.md`). The
commands and conventions below mirror `docs/development.md`; when rules change,
update both.

## Build & Test Commands

```bash
cargo build                                    # Build all crates
cargo test --workspace                         # Run all tests (some need network)
cargo test -p doggy-dns-nacos                 # Test a single crate
cargo test -p doggy-dns-plugin --test authority_chain_test  # Run a single test file
cargo test -p doggy-dns-basic --test config_test -- parse_full_config  # Run a single test
cargo clippy --all-features --all-targets -- -D warnings    # Lint (CI uses -D, not -W)
cargo fmt --all -- --check                     # Format check
cargo bench --no-run --workspace               # Verify benchmarks compile
cargo bench --workspace                        # Run all benchmarks (release profile: LTO, 1 codegen unit)
cargo bench -p doggy-dns-core                 # Run handler pipeline benchmarks only
cargo bench --bench nacos_bench                # Run a single benchmark file
cargo bench --bench handler_bench -- handler_pipeline_nacos_hit  # Filter by name
cargo tarpaulin --config tarpaulin.toml        # Coverage (XML + HTML reports)
cargo tarpaulin --workspace --out Stdout       # Coverage quick summary
DOGGY_NACOS_ADDR=127.0.0.1:8848 cargo test --test nacos_e2e_test -- --ignored  # Live-Nacos e2e (needs a standalone Nacos server)
cargo build --features xdp                 # Build with XDP support (requires Linux + bpf-linker)
cd xdp && cargo build --target bpfel-unknown-none --release  # Build eBPF program
cargo test -p doggy-dns-xdp                # Test XDP userspace crate
cargo bench -p doggy-dns-xdp               # Benchmark XDP components
sudo env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored  # XDP integration tests (requires root)
```

## Architecture

### Crate Dependency Graph

```
doggy-dns (binary)
  ├── doggy-dns-core            (server, handler, middleware; config re-exported from basic)
  │     ├── doggy-dns-basic
  │     └── doggy-dns-plugin
  ├── doggy-dns-nacos           (Nacos service discovery backend)
  │     ├── doggy-dns-basic
  │     └── doggy-dns-plugin
  ├── doggy-dns-observability   (OTLP logs/metrics/traces/profiles)
  │     ├── doggy-dns-basic
  │     └── doggy-dns-plugin
  ├── doggy-dns-control         (service control plane: MCP tools + health probes)
  │     └── doggy-dns-basic
  ├── doggy-dns-basic           (TOML config types, load & validate, wall-clock helpers — serde/toml/anyhow only)
  ├── doggy-dns-plugin          (traits, authority chain, built-in authorities — leaf crate)
  └── doggy-dns-xdp (optional, feature = "xdp")
        ├── doggy-dns-basic
        └── doggy-dns-plugin

xdp/
└── doggy-dns-xdp-ebpf  (eBPF program, separate workspace, target = bpfel-unknown-none)
```

### Request Processing Pipeline

DNS query → **Middleware chain** (before_request) → **AuthorityChain** (sequential lookup) → **Middleware chain** (after_request) → DNS response.

**AuthorityChain** iterates `Vec<(String, Arc<dyn ZoneHandler>)>` in order. Each handler returns `Skip` (try next), `Continue(Ok/Err)`, or `Break(Ok/Err)` (stop). Returns a `ResolveResult` with `outcome` + `handler_name`. The handler maps outcomes to DNS response codes: `NoError`, `NXDomain` (lookup error), or `ServFail`; all-skipped queries return `NXDomain`.

**Middleware** trait (`plugin.rs`): `before_request()` returns `Continue` or `ShortCircuit`; `after_request()` receives elapsed time plus the final outcome (response code, authority, answer count).

### Authority Plugin Types

| Kind | Struct | Crate | What it does |
|------|--------|-------|-------------|
| `nacos` | `NacosAuthority` | doggy-dns-nacos | Resolves `<svc>.<group>.<ns>.<zone>` from DashMap cache fed by Nacos push events |
| `native` | `NativeAuthority` | doggy-dns-plugin | Forwards to OS system resolver (/etc/resolv.conf) |
| `forward` | `ForwardAuthority` | doggy-dns-plugin | Forwards to explicit upstream servers (e.g., 8.8.8.8) |

All implement hickory's `ZoneHandler` trait. Plugins are evaluated in `[[plugins]]` config order; first non-Skip wins.

### Nacos Integration Flow

`NacosServiceWatcher` answers from the Nacos client's naming cache and keeps no
instance data of its own. `NacosAuthority.lookup()` calls
`ServiceCatalog::instances(key)` on every in-zone query; the watcher splits the
key, checks group/namespace and `ServiceFilter`, subscribes the name if this
process never has (a `DashSet<String>` of subscribed names — the SDK offers no
way to ask, and keeps every listener it is handed), then reads
`get_all_instances(..., subscribe = true)`. The SDK answers from its naming
cache when it has an entry and asks the server otherwise, so the answer is
always the client's current view: a last-instance removal push empties that
cache and the next query answers `NXDOMAIN`. A subscribe call that fails is
forgotten so the next query retries it.

`ServiceFilter` gates names before any Nacos call (`service_pattern`, a regex
wrapped in whole-name anchors, default `[A-Za-z0-9_-]+` = bare application
names). A name the filter rejects is never cached or subscribed, so it resolves
to authoritative `NXDOMAIN` like any unknown service.

The subscription carries a no-op listener: instance data stays in the client's
cache (written before the SDK notifies anyone), so the listener exists only to
keep the key in the SDK's subscriber registry and silence its "not subscribed"
warning; the control plane reports the subscribed-name count, no push timestamp.
The client is built with `naming_load_cache_at_start(true)`
(a restart answers from the previous run's data) and
`naming_push_empty_protection(false)` (a last-instance disappearance must reach
the cache). `NXDOMAIN` covers both an unknown name and a known service with no
usable instance, because `get_all_instances` returns an empty list for both.

DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.` (zone membership by
`Name::zone_of`, key by `mapping::service_key_from_name`, split from the right so
service names may contain dots). `ServiceKey` is one composite lowercase string
(`service \u{1} group \u{1} namespace`) and `CachedInstance.ip` is an `IpAddr`, parsed
when a query reads the list; the authority never lowercases or re-parses per query because
the handler receives a `LowerName`. Only `A`/`AAAA` record types are served, filtered
by query type. The authority owns its whole zone: unknown services return
authoritative `NXDOMAIN` and missing families/types return `NODATA` instead of falling
through to later plugins.

### Answer Rotation

`handler::AnswerRotation` (built from `[server] rotate_answers`, default `true`)
cycles the answer records of any response with more than one record, one step
per rotated response on one process-wide cursor — not one per name, so no table
grows with the names seen. The cursor is shared by every name and query type and
advances only when rotation actually happens. It is applied in `DoggyDnsHandler`
right after `take_answer_records`, before the response and the XDP cache payload
are built, so all three authorities share one behaviour and single-record or
negative answers are untouched. Rotation is an in-place `rotate_left` on the Vec
the handler already owns, so it adds no allocation; `doggy-dns-nacos`'s
`alloc_guard_test` budgets one `authority.lookup` call, which runs upstream of
rotation and so cannot move. Every other handler test builds its handler with
`AnswerRotation::new(false)`, so order-sensitive assertions — including the
`wire_golden_test` bytes — stay pinned to the authority's own order; if a
rotation change moves those bytes, it changed the response, not the expectation.
The rotation itself is proven by tests that share one handler across several
queries and assert the whole answer sequence: on (the `nacos` move path and the
clone-collect path `native`/`forward` reach), off, and reaching the XDP cache
payload rather than only the sent response. With XDP enabled a cache hit replays
the order captured when the entry was written until its TTL expires (rotation
then only spreads misses).

### Server (`core/server.rs`)

`build_server()` binds UDP + TCP on the configured port. Optionally registers TLS (DoT, port 853) and HTTPS (DoH, port 443, path `/dns-query`) when `[server.tls]`/`[server.https]` are enabled. Uses `rustls` for TLS.

The tokio runtime is built manually in `main()` with configurable `worker_threads`.

## Enforced Coding Rules

### Disallowed APIs (`.clippy.toml`, enforced by `-D warnings` in CI)

- **No `.unwrap()`** — use `.expect("reason")` or `?` with `anyhow`. Exception: test files use `#![allow(clippy::disallowed_methods)]` at crate root.
- **No `std::time::Instant`** — use `tokio::time::Instant`
- **No `std::thread::sleep`** — use `tokio::time::sleep`
- **No `std::env::set_var`/`remove_var`** — data race risk in multi-threaded context

### Formatting

- `rustfmt.toml`: edition 2024, max_width 100
- Rust edition 2024, stable toolchain

### Immutability Pattern

Cache updates replace the entire Vec for a key (never mutate in place). Config structs are `Clone`. Use `Arc<DashMap>` for concurrent access.

## Configuration

TOML config at `config/doggy-dns.toml`. Key structs in `crates/doggy-dns-basic` (`doggy_dns_basic`): `DoggyDnsConfig`, `ServerConfig`, `PluginConfig` (flat — fields per kind), `PluginKind` enum (`Nacos`, `Native`, `Forward`). `doggy-dns-core` re-exports the module as `config`.

`validate_config()` checks: worker_threads > 0, TLS/HTTPS cert files exist when enabled, remote_config fields non-empty when enabled, plugin rules (one enabled plugin when `[[plugins]]` exists; Nacos connection fields, valid unique zone, `service_pattern` non-blank when given; `min_ttl <= max_ttl`; forward upstream is IP or IP:port), observability ranges/URLs (observability enabled, signal ranges, profile duration/frequency, log level, endpoint format), and control-plane rules when `[control]` is enabled (valid `listen_addr`; `endpoints` non-empty, unique, within `mcp|healthz|readyz`). Every config string may use `${ENV_VAR}`; expansion happens in `load_config()` before deserialisation and a missing variable is fatal (the raw, unexpanded file content is what the MCP `get_config` tool sees).

## Test Conventions

- Integration tests in `tests/` and `crates/*/tests/` (not inline `#[cfg(test)]` modules)
- Forward/native tests make real network calls
- Nacos tests use a fake catalog or a DashMap as the catalog (no Nacos server needed)
- Benchmarks in `crates/*/benches/` using criterion 0.8, parameterized over worker_threads 1..4
- Benchmark output starts with "running 0 tests" (from default harness) — this is normal, criterion results follow
