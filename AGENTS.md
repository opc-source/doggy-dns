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
cd xdp && cargo build --target bpfel-unknown-none --release  # eBPF crate only; userspace embeds build.rs's $OUT_DIR/ebpf copy
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
  ├── doggy-dns-testutil        (dev-only: wire builder, capturing response handler, zone mocks; publish = false, no crate depends on it in a product build)
  └── doggy-dns-xdp (optional, feature = "xdp")
        ├── doggy-dns-basic
        ├── doggy-dns-plugin
        └── doggy-dns-xdp-abi

xdp/
├── doggy-dns-xdp-abi   (BPF map layouts, no_std; also used by doggy-dns-xdp)
└── doggy-dns-xdp-ebpf  (eBPF program, separate workspace, target = bpfel-unknown-none)
```

### Request Processing Pipeline

DNS query → **Middleware chain** (before_request) → **AuthorityChain** (sequential lookup) → **Middleware chain** (after_request) → DNS response.

**AuthorityChain** iterates `Vec<(String, Arc<dyn ZoneHandler>)>` in order. Each handler returns `Skip` (try next), `Continue(Ok/Err)`, or `Break(Ok/Err)` (stop). Returns a `ResolveResult` with `outcome` + `handler_name`. The handler maps outcomes to DNS response codes: `NoError`, `NXDomain` (lookup error), or `ServFail`; all-skipped queries return `NXDomain`.

**Middleware** trait (`plugin.rs`): `before_request()` returns `Continue` or `ShortCircuit`; `after_request()` receives elapsed time plus the final outcome (response code, authority, answer count).

### Authority Plugin Types

| Kind | Struct | Crate | What it does |
|------|--------|-------|-------------|
| `nacos` | `NacosAuthority` | doggy-dns-nacos | Resolves `<svc>.<group>.<ns>.<zone>` from the Nacos client's naming cache |
| `native` | `NativeAuthority` | doggy-dns-plugin | Forwards to OS system resolver (/etc/resolv.conf) |
| `forward` | `ForwardAuthority` | doggy-dns-plugin | Forwards to explicit upstream servers (e.g., 8.8.8.8) |

All implement hickory's `ZoneHandler` trait. Plugins are evaluated in `[[plugins]]` config order; first non-Skip wins.

### Nacos Integration Flow

`NacosServiceWatcher` answers from the Nacos client's naming cache and keeps no
instance data of its own. `NacosAuthority.lookup()` splits the name itself —
zone membership, then the service name, with the group and namespace labels
checked against its own config — and calls
`ServiceCatalog::instances(service)` on every in-zone query; the watcher checks
`ServiceFilter`, then splits on its ledger of subscribed names (a
`DashSet<String>` — the SDK offers no way to ask, and keeps every listener it is
handed). `entry_of(ledgered, ever_confirmed, window_open)` then picks the source:

- a ledger name reads `get_all_instances(..., subscribe = true)`, which the SDK
  answers from its naming cache, so the answer is the client's current view: a
  last-instance removal push empties that cache and the next query answers
  `NXDOMAIN`.
- an unledgered name in a process the registry has never answered (start-up) gets
  that same cache read first and is confirmed only when the cache has nothing to
  serve — the disk-loaded cache is what answers names during a start-up outage.
- an unledgered name in a process the registry *has* answered is first confirmed
  with `get_all_instances(..., subscribe = false)`, the only shape in nacos-sdk
  0.8.3 that reports a failed request; a failed confirmation tries the client's
  cache before answering `SERVFAIL`.
- an unledgered name inside an open backoff window gets the cache read and nothing
  else; an empty read is `SERVFAIL`, refused without waiting on the registry.

An empty list is never served from the cache — there it means "not held", not NODATA —
so a name answers either with instances or with the server's own verdict. The ledger
entry appears only after `subscribe` returns, so concurrent first queries can each take
the unledgered path until it lands.

nacos-sdk 0.8.3 gives `subscribe` nothing to report back — it registers the
listener and its own retry task, swallows the request error and returns `Ok` — so
the arm that forgets a failed subscribe is defensive, not a live path. A call that
fails, or one that takes longer than the watcher's three-second bound
(`NACOS_LOOKUP_TIMEOUT`), answers `SERVFAIL` instead of `NXDOMAIN`, so a Nacos
outage is not negative-cached downstream. The cache read taken *instead of* a call
carries its own 50-millisecond bound (`NACOS_CACHE_READ_TIMEOUT`); the ledgered
path keeps the three seconds, because there the SDK turns a cache miss into a fetch
of its own. `FailureBackoff` counts only calls that reached Nacos:
`NACOS_FAILURE_THRESHOLD` in a row open a `NACOS_FAILURE_BACKOFF` window, which
stays open only while the streak still reaches the threshold — a server-confirmed
answer restarts it even when a concurrent failure had just stamped the window — and
which a clock stepping behind the window's start lifts rather than extends. Inside
it an unledgered name is served from the client's cache or refused with `SERVFAIL`,
never after a wait on the registry, while ledgered names keep reading that cache. A
server-confirmed answer calls `record_success` — closing the window, restarting the
streak, and setting once for the process the `ever_confirmed` flag that turns the
start-up rule into confirm-first; a client-cache read calls neither, because it
proves nothing about the server. All four are constants, not config.
Known limit: the naming cache is written only by a push or the periodic refresh, so a
ledgered name whose entry has not been written yet re-fetches inline on the
`subscribe = true` path, where it waits out the same three-second bound even
inside the window. Against a registry that hangs, that answers `SERVFAIL` and
counts; against one that answers with an error, the SDK swallows it into an empty
list, and the query reads `NXDOMAIN` uncounted.

`ServiceFilter` gates names before any Nacos call (`service_pattern`, a regex
wrapped in whole-name anchors, default `[A-Za-z0-9_-]+` = bare application
names). A name the filter rejects is never cached or subscribed, so it resolves
to authoritative `NXDOMAIN` like any unknown service.

The subscription carries a no-op listener: instance data stays in the client's
cache (written before the SDK notifies anyone), so the listener exists only to
keep the key in the SDK's subscriber registry and silence its "not subscribed"
warning; the control plane reports the subscribed-name count, no push timestamp.
The client is built with `naming_load_cache_at_start(true)` (what a restart reads
back is the previous run's data, and until the registry has answered this process it
is also what a first query for a name reads; an address that *refuses* the connection
is tolerated the same way, but one
that accepts TCP and never answers blocks `NamingServiceWatcher::new` in the SDK's
unbounded `build()` health check, before the DNS port binds — pre-existing, and
unchanged here) and
`naming_push_empty_protection(false)` (a last-instance disappearance must reach
the cache). `NXDOMAIN` covers both an unknown name and a known service with no
usable instance, because `get_all_instances` returns an empty list for both.

DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.` (zone membership by
`Name::zone_of`, service name by `mapping::service_name_from_name`, split from the
right so service names may contain dots and the two labels below it must equal the
configured group and namespace). That function returns a `Cow<str>` and the catalog
is keyed by the service name alone: a single-label name — the shape the default
`service_pattern` selects — borrows straight out of the `LowerName` instead of
building a key, and one plugin serves one group/namespace pair, so nothing needs to
distinguish them at the key. `CachedInstance.ip` is an `IpAddr`, parsed
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

An instance list is replaced wholesale, never mutated in place (the Nacos client's naming cache does this on each push; we keep no copy). Config structs are `Clone`. Use `Arc<DashMap>` / `Arc<DashSet>` for concurrent access.

## Configuration

TOML config at `config/doggy-dns.toml`. Key structs in `crates/doggy-dns-basic` (`doggy_dns_basic`): `DoggyDnsConfig`, `ServerConfig`, `PluginConfig` (flat — fields per kind), `PluginKind` enum (`Nacos`, `Native`, `Forward`). `doggy-dns-core` re-exports the module as `config`.

`validate_config()` checks: worker_threads > 0, TLS/HTTPS cert files exist when enabled, remote_config fields non-empty when enabled, plugin rules (at least one enabled plugin when `[[plugins]]` exists; Nacos connection fields, valid unique zone, `service_pattern` non-blank when given; `min_ttl <= max_ttl`; forward upstream is IP or IP:port), observability ranges/URLs (observability enabled, signal ranges, profile duration/frequency, log level, endpoint format), and control-plane rules when `[control]` is enabled (valid `listen_addr`; `endpoints` non-empty, unique, within `mcp|healthz|readyz`). Every config string may use `${ENV_VAR}`; expansion happens in `load_config()` before deserialisation and a missing variable is fatal (the raw, unexpanded file content is what the MCP `get_config` tool sees).

## Test Conventions

- Integration tests in `tests/` and `crates/*/tests/` (not inline `#[cfg(test)]` modules)
- Forward/native tests make real network calls
- Nacos tests use a fake catalog or a DashMap as the catalog (no Nacos server needed)
- Benchmarks in `crates/*/benches/` using criterion 0.8, parameterized over worker_threads 1..4
- Benchmark output starts with "running 0 tests" (from default harness) — this is normal, criterion results follow
