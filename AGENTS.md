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

`NacosServiceWatcher` → subscribes to Nacos naming service → `InstanceChangeListener`
receives push events → replaces entire `Vec<CachedInstance>` for a `ServiceKey` in
`DashMap` (immutable update) → `NacosAuthority.lookup()` reads through the `DashMap`
read guard and builds records from borrowed instances.

DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.` (zone membership by
`Name::zone_of`, key by `mapping::service_key_from_name`, split from the right so
service names may contain dots). `ServiceKey` is one composite lowercase string
(`service \u{1} group \u{1} namespace`) and `CachedInstance.ip` is an `IpAddr`, parsed
when the push arrives; the authority never lowercases or re-parses per query because
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

### Config Hot Reload

A running process rebuilds its query path on `SIGHUP` (re-read the file it
started with) and, when `[remote_config]` is enabled, on a push from the Nacos
config service. Both go through `ReloadOrchestrator::apply` (`src/reload.rs`):
read the candidate (`load_config` / `parse_config` — the same expansion and
validation either way), refuse it if anything outside the query path changed,
rebuild the chain, swap.

The swap points exist because hickory's `Server` owns the `RequestHandler` by
value: the handler can never be replaced, only what it reads through. So
`DoggyDnsHandler` keeps everything a query reads from config — the authority
chain and the rotation flag — in one `ArcSwap<QueryPath>`, and hands a
`ReloadHandle` out before `build_server`; that handle has a single method, one
store, so a reload is never observable as two changes. `middleware.logging` is
the exception and stays an `AtomicBool` inside an always-installed logging
middleware — the middleware chain owns the live XDP cache map and the OTel
instruments, so it is never rebuilt. The rotation *cursor* is not part of the
swapped snapshot either: it is a counter, and surviving the swap is the point.
When adding a reloadable setting, follow that shape: put it behind the shared
`ArcSwap` if the query path reads it, do not try to construct a new handler.

`doggy_dns_basic::reload::reload_conflicts` is the allow-list: `[[plugins]]`,
`server.rotate_answers` and `middleware.logging` may differ, everything else
refuses the **whole** reload (a half-applied config is the failure mode this
prevents). The rule is an allow-list on purpose — when a section becomes
reloadable, add it to the list *and* move its swap point behind the handle.
The list is written field by field and
`crates/doggy-dns-basic/tests/reload_compat_test.rs` has one case per refused
path, so **a field nobody added to either is reloadable by construction and is
then silently ignored**: the reload is accepted, the config in memory records
the new value, and nothing on the query path reads the difference. A new config
field therefore needs a `FORBIDDEN` case there or a swap point, deliberately.

Reloads are serialised (one `tokio::sync::Mutex` covers an apply) and coalesced
(500 ms window, newest request wins). Failure and rejection leave everything
alone: the old chain keeps answering and `generation` does not move, which is
what makes the counter mean "how many query paths this process served".

Nacos watchers are pooled across reloads (`doggy_dns_nacos::pool`): a watcher is
keyed by connection identity (`server_addr`, `namespace`, `group`) and *not* by
zone or TTL, so republishing a zone keeps the gRPC connection and its warm
cache. `plan_reuse` matching consumes, so two zones on one server take two
watchers. A reused watcher is deliberately not re-subscribed (the SDK's listener
set would grow per reload) and a watcher no plugin wants is not released (the
SDK has no shutdown), which is why `subscribe_all` semantics and the control
plane's per-zone live signals stay start-up facts until a restart.

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

Cache updates replace the entire Vec for a key (never mutate in place). Config structs are `Clone` and `PartialEq` — the reload compat check compares the running config with a candidate field by field, so a new config struct or field has to derive `PartialEq` (add it to the derive list; a plain `PartialEq` on a secret-carrying struct is fine, its redaction lives in `Debug`). Use `Arc<DashMap>` for concurrent access.

## Configuration

TOML config at `config/doggy-dns.toml`. Key structs in `crates/doggy-dns-basic` (`doggy_dns_basic`): `DoggyDnsConfig`, `ServerConfig`, `PluginConfig` (flat — fields per kind), `PluginKind` enum (`Nacos`, `Native`, `Forward`). `doggy-dns-core` re-exports the module as `config`.

`validate_config()` checks: worker_threads > 0, TLS/HTTPS cert files exist when enabled, remote_config fields non-empty when enabled, plugin rules (one enabled plugin when `[[plugins]]` exists; Nacos connection fields, valid unique zone; `min_ttl <= max_ttl`; forward upstream is IP or IP:port), observability ranges/URLs (observability enabled, signal ranges, profile duration/frequency, log level, endpoint format), and control-plane rules when `[control]` is enabled (valid `listen_addr`; `endpoints` non-empty, unique, within `mcp|healthz|readyz`). Every config string may use `${ENV_VAR}`; expansion happens in `load_config()` before deserialisation and a missing variable is fatal (the raw, unexpanded file content is what the MCP `get_config` tool sees).

`parse_config(content)` is the same pipeline without the filesystem — it is what a hot reload runs on pushed content, so remote and local config can never diverge in validation. `reload_conflicts(old, new)` returns the config paths whose difference forbids an in-place reload; see [Config Hot Reload](#config-hot-reload).

## Test Conventions

- Integration tests in `tests/` and `crates/*/tests/` (not inline `#[cfg(test)]` modules)
- Forward/native tests make real network calls
- Nacos tests use mock DashMap caches (no Nacos server needed)
- Hot reload is tested per layer, so a failure names the layer it broke: the
  allow-list (`crates/doggy-dns-basic/tests/reload_compat_test.rs`, one case per
  refused config path), the swap (`crates/doggy-dns-core/tests/handler_reload_test.rs`,
  against a handler that is already answering), the reuse decision
  (`crates/doggy-dns-nacos/tests/pool_test.rs`, pure), the orchestrator's
  decisions (`tests/reload_orchestrator_test.rs`, forward-only chains so no
  network) and the triggers end to end (`tests/nacos_e2e_test.rs`)
- Benchmarks in `crates/*/benches/` using criterion 0.8, parameterized over worker_threads 1..4 where the measurement is a request pipeline; `doggy-dns-core`'s `query_path_bench` also has `query_path_read`, a single-threaded decomposition of the per-query snapshot read (`plain`/`clone`/`swap`/`cached`), which is priced without a runtime because a thread knob there would measure tokio's task machinery, not the refcount traffic
- Shared bench helpers live in `crates/doggy-dns-core/benches/common/mod.rs` (same `mod common;` pattern as `crates/doggy-dns-observability/tests/common/`)
- Benchmark output starts with "running 0 tests" (from default harness) — this is normal, criterion results follow
