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
| `nacos` | Resolves service names from the Nacos client's naming cache. Format: `<service>.<group>.<namespace>.<zone>.` |
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
- One rule picks the source per query (`entry_of`): a name this process subscribed
  reads the client's cache; an unledgered name the registry has never answered
  (start-up) reads the cache first and is confirmed by the server only if the cache
  has nothing; an unledgered name in a process the registry *has* answered is
  confirmed first (`get_all_instances` with `subscribe = false`), with the client's
  cache as the fallback if that call failed; and an unledgered name inside the
  backoff window gets the cache read only. Only names the `service_pattern`
  selects get this far. Every answer needs instances: an empty cache read is a miss,
  not a `NODATA`, so no query is served from absence. The ledger entry appears only
  once `subscribe` returns, so concurrent first queries for one name may each take
  the unledgered path until the entry lands.
- Keeps no instance table of its own: every answer is the Nacos client's naming cache
  (`subscribe = true`), which each push replaces wholesale. `CachedInstance.ip` is an
  `IpAddr`, so a query does its address parsing once per read list.
- Three failed calls to Nacos in a row open a 30-second window (`FailureBackoff`)
  in which an unledgered name is served from the client's cache or refused with
  `SERVFAIL` — never after a wait on the registry; an answer that did come from the
  server closes it. The window needs both the timestamp and the streak. Each call
  that reaches Nacos is bounded at three seconds; a cache read taken *instead of* a
  call is bounded at 50 milliseconds, while the subscribed path keeps the three
  seconds because the SDK turns a cache miss into its own fetch.
- The catalog is keyed by the bare service name. One plugin serves one
  group/namespace pair and the authority checks those two labels against its own
  config before asking for it, so the key never has to carry them.
- The apex `SOA` is built once at authority construction and cached again on the
  authority chain entry, so a negative answer costs no extra lookup after the first.
- DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.`
- Example: `dig user-service.DEFAULT_GROUP.public.nacos.local A`
- The name is split from the right, so the mapping itself accepts dotted service
  names (`com.example.api.DEFAULT_GROUP.public.nacos.local`) — which the default
  `service_pattern` rejects, so serving one takes a widened pattern
- The authority is authoritative for its whole zone: once a name is inside
  `<zone_suffix>`, a miss never falls through to `native`/`forward`. An unknown
  service returns `NXDOMAIN`; a service with no instance of the requested
  family (or a non-`A`/`AAAA` query type) returns `NODATA`
- `A` queries return only `A` records and `AAAA` queries only `AAAA` records
- The apex answers `SOA` queries with a synthesized record (MNAME = zone,
  RNAME = `hostmaster.<zone>`, minimum TTL = the plugin `ttl`); the same record
  is attached to `NXDOMAIN`/`NODATA` responses

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
