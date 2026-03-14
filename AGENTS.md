# AGENTS.md

This file provides guidance to AI Coding when working with code in this repository.

## Build & Test Commands

```bash
cargo build                                    # Build all crates
cargo test --workspace                         # Run all tests (some need network)
cargo test -p dns-filter-nacos                 # Test a single crate
cargo test -p dns-filter-plugin --test authority_chain_test  # Run a single test file
cargo test -p dns-filter-core --test config_test -- parse_full_config  # Run a single test
cargo clippy --all-features --all-targets -- -D warnings    # Lint (CI uses -D, not -W)
cargo fmt --all -- --check                     # Format check
cargo bench --no-run --workspace               # Verify benchmarks compile
cargo bench                                    # Run benchmarks
```

## Architecture

### Crate Dependency Graph

```
dns-filter (binary)
  ├── dns-filter-core     (server, config, handler, middleware)
  │     └── dns-filter-plugin
  ├── dns-filter-nacos    (Nacos service discovery backend)
  │     └── dns-filter-plugin
  └── dns-filter-plugin   (traits, authority chain, built-in authorities — leaf crate)
```

### Request Processing Pipeline

DNS query → **Middleware chain** (before_request) → **AuthorityChain** (sequential lookup) → **Middleware chain** (after_request) → DNS response.

**AuthorityChain** iterates `Vec<(String, Arc<dyn ZoneHandler>)>` in order. Each handler returns `Skip` (try next), `Continue(Ok/Err)`, or `Break(Ok/Err)` (stop). Returns a `ResolveResult` with `outcome` + `handler_name`. The handler maps outcomes to DNS response codes: `NoError`, `ServFail`, or `NXDomain` (all skipped).

**Middleware** trait (`plugin.rs`): `before_request()` returns `Continue` or `ShortCircuit`; `after_request()` receives elapsed time.

### Authority Plugin Types

| Kind | Struct | Crate | What it does |
|------|--------|-------|-------------|
| `nacos` | `NacosAuthority` | dns-filter-nacos | Resolves `<svc>.<group>.<ns>.<zone>` from DashMap cache fed by Nacos push events |
| `system_dns` | `SystemAuthority` | dns-filter-plugin | Forwards to OS system resolver (/etc/resolv.conf) |
| `forward` | `ForwardAuthority` | dns-filter-plugin | Forwards to explicit upstream servers (e.g., 8.8.8.8) |

All implement hickory's `ZoneHandler` trait. Plugins are evaluated in `[[plugins]]` config order; first non-Skip wins.

### Nacos Integration Flow

`NacosServiceWatcher` → subscribes to Nacos naming service → `InstanceChangeListener` receives push events → replaces entire `Vec<CachedInstance>` for a `ServiceKey` in `DashMap` (immutable update) → `NacosAuthority.lookup()` reads from cache.

DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.` (parsed by `mapping::parse_dns_name`). Only `A`/`AAAA` record types handled.

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

TOML config at `config/dns-filter.toml`. Key structs in `core/config.rs`: `DnsFilterConfig`, `ServerConfig`, `PluginConfig` (flat — fields per kind), `PluginKind` enum (`Nacos`, `SystemDns`, `Forward`).

`validate_config()` checks: worker_threads > 0, TLS/HTTPS cert files exist when enabled, remote_config fields non-empty when enabled.

## Test Conventions

- Integration tests in `tests/` and `crates/*/tests/` (not inline `#[cfg(test)]` modules)
- Forward/system_dns tests make real network calls
- Nacos tests use mock DashMap caches (no Nacos server needed)
- Benchmarks in `crates/*/benches/` using criterion 0.8
