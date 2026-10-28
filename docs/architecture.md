# Architecture

Part of [README](../README.md).

## Crate dependency graph

- `doggy-dns` (binary) -> `doggy-dns-core`, `doggy-dns-plugin`, `doggy-dns-nacos`, `doggy-dns-observability`, `doggy-dns-mcp`
- `doggy-dns-core` -> `doggy-dns-basic`, `doggy-dns-plugin`
- `doggy-dns-nacos` -> `doggy-dns-plugin`
- `doggy-dns-observability` -> `doggy-dns-basic`, `doggy-dns-plugin`
- `doggy-dns-mcp` -> `doggy-dns-basic`
- `doggy-dns-basic` is the config leaf crate (no internal deps)
- `doggy-dns-plugin` is the leaf crate (no internal deps)

## Request flow

1. **Middleware chain** runs `before_request()` on each middleware in order. If any returns `ShortCircuit`, the request is refused immediately.
2. **Authority chain** iterates handlers sequentially. Each handler returns one of:
   - `Skip` -- does not handle this query, try the next handler
   - `Continue(Ok(records))` -- resolved, return records
   - `Continue(Err(e))` / `Break(Err(e))` -- authoritative failure; the lookup
     error maps to `NXDOMAIN` (`ResponseCode::NXDomain`), `NODATA`
     (`NameExists`), or `SERVFAIL` for anything else
3. If all handlers skip, the response is `NXDOMAIN`.
4. Middleware chain runs `after_request()` with elapsed time and the final outcome (response code, authority, answer count).

## Plugin axes

| Layer | Trait | Purpose | Implementations |
|-------|-------|---------|-----------------|
| Middleware | `Middleware` | Cross-cutting concerns | `LoggingMiddleware`, OTel metrics in `doggy-dns-observability` |
| Authority | `ZoneHandler` | DNS data sources | `NacosAuthority`, `NativeAuthority`, `ForwardAuthority` |

**Authority plugins:**

| Kind | Description |
|------|-------------|
| `nacos` | Resolves service names from Nacos registry via DashMap cache. Format: `<service>.<group>.<namespace>.<zone>.` |
| `native` | Resolves using the OS system resolver (`/etc/resolv.conf`) |
| `forward` | Resolves using explicit upstream DNS servers. Accepts `IP` or `IP:port` (port defaults to 53) and uses the configured port for both UDP and TCP |

Plugins are evaluated in the order defined in the `[[plugins]]` config array. The first handler that returns a non-Skip result wins.

`ForwardAuthority` keeps upstream semantics: `NXDOMAIN` and `NODATA` responses
are returned as authoritative negatives, upstream `SERVFAIL`/`REFUSED` keep
their code, and transport failures (timeout, I/O) become `SERVFAIL` rather than
silently falling through or turning into `NXDOMAIN`. `NativeAuthority` falls
through on lookup failure so a later `forward` plugin can still answer.

## Nacos integration

- Connects to Nacos naming service via `nacos-sdk`
- Subscribes to all services on startup; receives push updates on instance changes
- Maintains an in-memory `DashMap<ServiceKey, Vec<CachedInstance>>` cache; updates replace the entire `Vec` for a key (immutable update)
- DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.`
- Example: `dig user-service.DEFAULT_GROUP.public.nacos.local A`
- The name is split from the right, so service names may contain dots
  (`com.example.api.DEFAULT_GROUP.public.nacos.local`)
- The authority is authoritative for its whole zone: once a name is inside
  `<zone_suffix>`, a miss never falls through to `native`/`forward`. An unknown
  service returns `NXDOMAIN`; a service with no instance of the requested
  family (or a non-`A`/`AAAA` query type) returns `NODATA`
- `A` queries return only `A` records and `AAAA` queries only `AAAA` records

## Server protocols

| Protocol | Default Port | Description |
|----------|-------------|-------------|
| UDP | config `port` | Standard DNS |
| TCP | config `port` | Standard DNS over TCP |
| TLS (DoT) | 853 | DNS over TLS, requires `[server.tls]` |
| HTTPS (DoH) | 443 | DNS over HTTPS at `/dns-query`, requires `[server.https]` |

The tokio runtime is built manually in `main()` with configurable `worker_threads`.

## Logging events

Structured logging via `tracing`. Key log events:

| Event | Level | Fields |
|-------|-------|--------|
| Query resolved | `INFO` | `query`, `qtype`, `authority`, `answers`, `duration_ms`, `response_code` |
| No authority handled query | `INFO` | `query`, `qtype`, `response_code=NXDomain` |
| Authority returned negative response | `DEBUG` | `query`, `qtype`, `authority`, `response_code=NXDomain/NoError` |
| Authority returned error | `WARN` | `query`, `qtype`, `authority`, `error`, `response_code=ServFail` |
| Middleware short-circuit | `WARN` | `duration_ms`, `response_code=Refused` |
| Configuration loaded | `INFO` | `listen_addr`, `port`, `worker_threads`, `plugin_count`, ... |

Control log level via the `RUST_LOG` environment variable. OTLP export of the
same events is covered in [observability.md](observability.md).
