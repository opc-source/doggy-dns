# Architecture

Part of [README](../README.md).

## Crate dependency graph

- `doggy-dns` (binary) -> `doggy-dns-core`, `doggy-dns-plugin`, `doggy-dns-nacos`, `doggy-dns-observability`, `doggy-dns-control`
- `doggy-dns-core` -> `doggy-dns-basic`, `doggy-dns-plugin`
- `doggy-dns-nacos` -> `doggy-dns-basic`, `doggy-dns-plugin`
- `doggy-dns-observability` -> `doggy-dns-basic`, `doggy-dns-plugin`
- `doggy-dns-control` -> `doggy-dns-basic`
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
4. **Response flags and negative caching**: answers from a zone-owning handler
   (`ZoneType::Primary`) set `AA`, clear `RA`, and carry the zone apex `SOA`
   in negative responses so downstream resolvers can cache them (RFC 2308).
   External handlers (forward/native) clear `AA` and set `RA`.
5. **Answer rotation** (`[server] rotate_answers`, default on) cycles the answer
   records of any response with more than one record, one step per rotated
   response, so clients that use the first record spread across instances. It
   runs in the handler, after the authority chain and before both the response
   and the XDP cache payload are built, so every authority gets it and
   single-record and negative answers are untouched.
6. Middleware chain runs `after_request()` with elapsed time and the final outcome (response code, authority, answer count).

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
- Maintains an in-memory `DashMap<ServiceKey, Vec<CachedInstance>>` cache; updates
  replace the entire `Vec` for a key (immutable update). `ServiceKey` is a single
  composite lowercase string and `CachedInstance.ip` is an `IpAddr`, so a query builds
  one key and does no address parsing.
- The apex `SOA` is built once at authority construction and cached again on the
  authority chain entry, so a negative answer costs no extra lookup after the first.
- DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.`
- Example: `dig user-service.DEFAULT_GROUP.public.nacos.local A`
- The name is split from the right, so service names may contain dots
  (`com.example.api.DEFAULT_GROUP.public.nacos.local`)
- The authority is authoritative for its whole zone: once a name is inside
  `<zone_suffix>`, a miss never falls through to `native`/`forward`. An unknown
  service returns `NXDOMAIN`; a service with no instance of the requested
  family (or a non-`A`/`AAAA` query type) returns `NODATA`
- `A` queries return only `A` records and `AAAA` queries only `AAAA` records
- The apex answers `SOA` queries with a synthesized record (MNAME = zone,
  RNAME = `hostmaster.<zone>`, minimum TTL = the plugin `ttl`); the same record
  is attached to `NXDOMAIN`/`NODATA` responses

## Config hot reload

hickory's `Server` takes the `RequestHandler` by value, so the handler a running
process serves from can never be replaced — only what it reads through.
`DoggyDnsHandler` therefore keeps the authority chain in an `ArcSwap` and hands
out a `ReloadHandle` (that chain plus the rotation cursor) before the server is
built, and the two remaining reloadable settings are flags rather than structures:
`server.rotate_answers` gates the shared `AnswerRotation`, and `middleware.logging`
gates a `LoggingToggle` inside a logging middleware that is installed
unconditionally — the middleware chain owns the live XDP cache map and the OTel
instruments, so rebuilding it on a reload would rebuild the kernel handle and
the metric instruments too. A swap is atomic and in-flight queries hold the chain
they started on.

`ReloadOrchestrator` (`src/reload.rs`, reachable because the binary package also
has a lib target) is the single decision point both triggers funnel into: read the
candidate (`load_config` for `SIGHUP`, `parse_config` for a pushed string that
never touches a filesystem — the same expansion and validation either way), ask
`doggy_dns_basic::reload::reload_conflicts` whether anything outside the
query path changed, rebuild the chain, swap. The rules and the reason they are an
allow-list live in
[configuration.md](configuration.md#hot-reload).

- **Serialised.** One `tokio::sync::Mutex` covers the whole apply, so a `SIGHUP`
  racing a push cannot build two chains at once; each reload applies against the
  config the previous one installed. Requests are coalesced over a 500 ms window
  and the newest wins, because it carries the newest intent.
- **All or nothing.** A conflict rejects the candidate whole and leaves the
  running path untouched; an unreadable, invalid or unbuildable candidate fails
  the same way. The generation counter only moves on an applied reload, so it
  keeps meaning "how many query paths this process has served".
- **Chain building lives in the binary.** `doggy-dns-core` does not depend on
  `doggy-dns-nacos`, so `build_chain` sits with the orchestrator and core only
  exposes the swap.
- **Nacos watchers are pooled.** A watcher owns a gRPC connection and a warm
  instance cache, and rebuilding it is the expensive part of a reload, so
  `doggy_dns_nacos::pool` keys it by connection identity (`server_addr`,
  `namespace`, `group`) and `plan_reuse` says which pooled watcher serves which
  plugin — matching consumes, so two zones on one server take two watchers.
  Zone and TTL are excluded from the identity: republishing a zone keeps the
  connection and everything already pushed. A released watcher cannot be closed
  (the SDK exposes no shutdown), so an unused one stays pooled until a restart.

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
