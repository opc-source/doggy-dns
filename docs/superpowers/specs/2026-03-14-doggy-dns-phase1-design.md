# doggy-dns Phase 1 Design Spec

## Overview

doggy-dns is a Rust-based service discovery DNS server built on `hickory-dns` (v0.26.0-beta.1). It provides a plugin-based middleware architecture inspired by CoreDNS, with Nacos as the first backend for service instance resolution.

**Phase 1 scope**: DNS server core + plugin middleware chain + Nacos backend + system DNS + upstream forwarding.

## Goals

- Resolve service names registered in Nacos to A/AAAA records via DNS
- Support UDP, TCP, DNS-over-TLS (DoT), and DNS-over-HTTPS (DoH)
- Plugin chain architecture where each plugin can handle, decline, or short-circuit a query
- Dynamic configuration via Nacos config service with hot-reload
- TOML-based local configuration with sensible defaults

## Non-Goals (Phase 1)

- SRV, TXT, or CNAME record generation from Nacos
- Kubernetes, etcd, Redis, or SQL backends (future phases)
- DNSSEC signing
- Web UI / dashboard
- DNS zone file hosting
- Rate limiting middleware (future phase)
- Prometheus/HTTP metrics export endpoint (future phase)

## Architecture

### Hybrid Two-Layer Pipeline

The server uses a hybrid architecture with two distinct layers:

**Layer 1 -- DoggyDnsHandler (implements `RequestHandler`):**
The outer layer owns cross-cutting middleware concerns. Each middleware can inspect/modify the request, short-circuit (e.g., rate limiter denies), or pass through to the next layer.

**Layer 2 -- Authority Chain (using `LookupControlFlow`):**
The inner layer is a chain of Authority implementations. Each authority either resolves the query and returns records, or declines via `LookupControlFlow` to pass the query to the next authority. The chain is managed by a custom `AuthorityChain` struct (not hickory's `Catalog`) that holds a `Vec<Box<dyn Authority>>` and iterates through them in order, stopping at the first non-decline response.

**Handoff between layers:** `DoggyDnsHandler.handle_request()` runs the outer middleware chain. If no middleware short-circuits, it calls `self.authority_chain.resolve(request)`, which iterates through the authorities. The authority chain converts the hickory `Request` into a zone lookup, calling each authority's `search()` method until one returns `LookupControlFlow::Continue` (with records) or all decline.

```
Incoming DNS Query (UDP / TCP / DoT / DoH)
            |
            v
  DoggyDnsHandler (RequestHandler)
  +--------------------------------------+
  | Outer Middleware Chain               |
  |   Logger -> Metrics                  |
  +-------------------|------------------+
                      |
                      v
  +--------------------------------------+
  | Inner: AuthorityChain                |
  |   NacosAuthority                     |
  |     -> NativeAuthority               |
  |       -> ForwardAuthority            |
  +--------------------------------------+
```

**Authority chain order:**
1. `NacosAuthority` -- resolves from Nacos service registry. Declines if service not found.
2. `NativeAuthority` -- uses hickory-resolver configured from `/etc/resolv.conf` to query system nameservers. Declines if not found.
3. `ForwardAuthority` -- forwards to configurable upstream DNS (default: 8.8.8.8, 1.1.1.1) as last resort.

## Project Structure

```
doggy-dns/
  Cargo.toml                  # Workspace root
  crates/
    doggy-dns-core/          # Server runtime, config, middleware chain
      Cargo.toml
      src/
        lib.rs
        config.rs             # TOML config parsing & validation
        server.rs             # ServerFuture setup (UDP/TCP/DoT/DoH)
        handler.rs            # DoggyDnsHandler (RequestHandler impl)
        middleware/
          mod.rs
          chain.rs            # Middleware chain orchestration
          logging.rs          # Request/response logging
          metrics.rs          # Query metrics collection

    doggy-dns-plugin/        # Plugin trait definitions, registry, built-in authorities
      Cargo.toml
      src/
        lib.rs
        plugin.rs             # Plugin trait (cross-cutting middleware)
        authority.rs          # AuthorityPlugin trait (data-source factory)
        authority_chain.rs    # AuthorityChain: iterates Vec<Box<dyn Authority>>
        registry.rs           # Plugin discovery & instantiation from config
        builtin/
          mod.rs
          native.rs       # NativeAuthority (wraps AsyncResolver + /etc/resolv.conf)
          forward.rs          # ForwardAuthority (wraps AsyncResolver + upstream IPs)

    doggy-dns-nacos/         # Nacos service discovery plugin
      Cargo.toml
      src/
        lib.rs
        authority.rs          # NacosAuthority implementing hickory Authority
        watcher.rs            # Nacos service subscription & cache sync
        mapping.rs            # Service instance -> DNS record mapping
        config_watcher.rs     # Nacos config listener for remote hot-reload

  src/
    main.rs                   # Binary: parse config, build chain, start server

  config/
    doggy-dns.toml           # Example configuration file
```

### Crate Responsibilities

**doggy-dns-core**: Owns `DoggyDnsHandler`, the outer middleware chain, server lifecycle (bind sockets, register listeners), TOML config parsing, and TLS setup. Depends on hickory-server and hickory-proto.

**doggy-dns-plugin**: Defines the `Plugin` trait for cross-cutting middleware and the `AuthorityPlugin` trait for data-source plugins. Contains the plugin registry that instantiates plugins from config, plus built-in authorities (`NativeAuthority`, `ForwardAuthority`) that have no external backend dependencies. Has no heavy dependencies -- only hickory-proto and hickory-resolver.

The `AuthorityPlugin` trait is a factory interface: given a plugin config section, it produces a `Box<dyn Authority>`. Each backend crate (e.g., `doggy-dns-nacos`) implements `AuthorityPlugin` to create its authority. The `Plugin` trait is for outer middleware (logging, metrics) and wraps the request processing with before/after hooks.

**Built-in authorities in doggy-dns-plugin:**

- `NativeAuthority`: Wraps `hickory_resolver::AsyncResolver` configured from `/etc/resolv.conf`. The resolver has its own internal cache (sized by the `cache_size` config parameter). When a query arrives, it delegates to the async resolver. If the resolver returns NXDOMAIN or times out, it declines via `LookupControlFlow`.
- `ForwardAuthority`: Wraps `hickory_resolver::AsyncResolver` configured with explicit upstream server IPs from config. Similar to NativeAuthority but uses hardcoded upstreams instead of system resolv.conf. Has its own cache. Acts as the last-resort fallback.

**doggy-dns-nacos**: Implements `NacosAuthority` backed by `nacos-sdk`. Maintains an in-memory cache (`DashMap`) of service instances, synced via Nacos push notifications. Also provides `NacosConfigWatcher` for remote config hot-reload.

## Nacos Integration

### Service Resolution

`NacosAuthority` resolves DNS queries by mapping domain names to Nacos service instances.

**DNS naming convention:**
```
<service-name>.<group>.<namespace>.nacos.local
```

Example: `user-service.DEFAULT_GROUP.public.nacos.local` resolves to `10.0.1.5, 10.0.1.6`

**Resolution flow:**
1. Parse incoming query name to extract service-name, group, and namespace
2. Look up `(service-name, group, namespace)` in the local `DashMap` cache
3. If found: return A records (IPv4) and/or AAAA records (IPv6) for each instance IP
4. If not found: decline via `LookupControlFlow`, passing to the next authority

### Cache Strategy

- On startup: connect to Nacos server, subscribe to all services in the configured namespace/group
- Cache: `DashMap<ServiceKey, Vec<ServiceInstance>>` where `ServiceKey = (service_name, group, namespace)`
- Updates: `nacos-sdk` pushes service change events; the watcher updates the cache immutably (replace entire Vec for a key, never mutate in place)
- TTL: configurable per-record TTL (default 6s) applied to generated DNS records

### Remote Config

When `[remote_config]` is enabled:
1. On startup, load local `doggy-dns.toml` as initial config
2. Subscribe to Nacos config service for `data_id` in the configured namespace/group
3. On config change: parse new TOML, validate, rebuild plugin chain
4. Hot-swap the active chain using `ArcSwap<PluginChain>`
5. Local config serves as fallback if Nacos is unreachable

## Configuration

```toml
[server]
listen_addr = "0.0.0.0"
port = 53
tcp_timeout = 10

[server.tls]
enabled = false
port = 853
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[server.https]
enabled = false
port = 443
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[middleware]
logging = true
metrics = false

# Plugins are evaluated in order (chain).
# Each plugin kind maps to an authority implementation.

[[plugins]]
kind = "nacos"
enabled = true
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "native"
enabled = true
cache_size = 256
min_ttl = 10
max_ttl = 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53", "1.1.1.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600

# Remote config: listen for config changes from Nacos.
# Local config is the initial/fallback config.
[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DOGGY_DNS"
data_id = "doggy-dns.toml"
```

All sections have sensible defaults. `[server]` is required. If no `[[plugins]]` are configured, the server defaults to a single `forward` plugin with upstream `["8.8.8.8:53", "1.1.1.1:53"]` so it can still resolve queries.

## Error Handling

- **DNS errors**: Map to appropriate RCODE (SERVFAIL for internal errors, NXDOMAIN for not found, REFUSED for policy denial)
- **Plugin failures**: Log error, continue chain to next plugin (fail-open for resolution)
- **Nacos connection failures**: Serve from cached data, log warnings, retry connection in background with exponential backoff
- **Config parse errors**: Fail fast at startup with clear error messages pointing to the problematic field
- **Remote config errors**: Log warning, keep current config, retry on next push

## Graceful Shutdown

The server listens for SIGTERM and SIGINT (via `tokio::signal`). On signal:
1. Stop accepting new connections
2. Drain in-flight DNS requests (with a configurable timeout, default 5s)
3. Close Nacos client connections and unsubscribe from service/config watchers
4. Log shutdown completion and exit

## Observability

**Logging**: Uses `tracing` with `tracing-subscriber`. Configured via `RUST_LOG` env var (e.g., `RUST_LOG=doggy_dns=info`). Default format is human-readable for development; structured JSON output can be enabled via config (`[middleware] log_format = "json"`).

**Metrics** (Phase 1 scope: internal counters only, no export endpoint):
- `dns_queries_total` -- counter by query type (A, AAAA, etc.)
- `dns_query_duration_seconds` -- histogram of query processing time
- `dns_responses_by_rcode` -- counter by response code
- `plugin_hits` -- counter per plugin (which authority resolved the query)
- Metrics are logged periodically at INFO level. A Prometheus/HTTP export endpoint is deferred to a later phase.

## Dependencies

```toml
# DNS
hickory-server = "0.26.0-beta.1"
hickory-proto = "0.26.0-beta.1"
hickory-resolver = "0.26.0-beta.1"

# Nacos
nacos-sdk = "0.6"

# Async runtime
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"

# Config
serde = { version = "1", features = ["derive"] }
toml = "1.0"

# Concurrency
dashmap = "6"
arc-swap = "1"

# TLS
rustls = "0.23"

# Observability
tracing = "0.1"
tracing-subscriber = "0.3"

# Error handling
anyhow = "1"
```

## Testing Strategy

**Unit tests** (per crate):
- Config parsing: valid/invalid TOML, default values, missing sections
- DNS record mapping: service instance to A/AAAA record conversion
- Middleware: logging output, metrics counters
- Plugin registry: correct instantiation from config

**Integration tests**:
- Full pipeline with mock Nacos server
- Authority chain: Nacos hit, Nacos miss -> system DNS, Nacos miss -> forward
- Hot-reload: config change triggers chain rebuild
- TLS handshake for DoT/DoH

**E2E tests**:
- Start doggy-dns server with test config
- Send DNS queries via hickory-client or `dig`
- Verify correct A/AAAA responses for Nacos-registered services
- Verify forwarding works for non-Nacos domains

**Target coverage**: 80%+

## Future Phases

- **Phase 2**: Kubernetes backend (`kube-rs` / `k8s-openapi`)
- **Phase 3**: etcd backend (`etcd-client`)
- **Phase 4**: Redis backend (`redis-rs`)
- **Phase 5**: SQL backend (`sqlx`)
- **Cross-cutting**: DNS caching plugin, ACL/blocklist plugin, health check plugin, DNSSEC signing
