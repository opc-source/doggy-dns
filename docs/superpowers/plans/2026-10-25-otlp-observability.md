# OTLP Observability → OpenObserve (Design + Implementation Plan)

**Goal:** Add first-class OTLP/HTTP export of logs, metrics, traces, and CPU profiles from doggy-dns to OpenObserve, while staying compatible with OTel Collector and Vector OTLP endpoints.

**Status:** implemented (2026-10-25)

## Decisions

1. **Single export target.** Fan-out is delegated to an OTel Collector/Vector when needed.
2. **HTTP/protobuf only in this phase.** gRPC is out of scope.
3. **Profiles use an in-process pprof → OTLP profiles exporter**, disabled by default.
4. **OTLP logs default to `warn`.** Configurable; stdout logging remains independent.
5. **Endpoint precedence:** per-signal config → top-level config → OTLP env vars → backend preset default.
6. **Default backend is OpenObserve:** `http://127.0.0.1:5080/api/default`.
7. **OpenObserve runs as an independent service** (no sidecar assumptions).

## Architecture

```
doggy-dns
  └── doggy-dns-observability
        ├── OTLP/HTTP logs      (tracing → opentelemetry-appender-tracing → /v1/logs)
        ├── OTLP/HTTP metrics   (DnsMetricsMiddleware → MeterProvider → /v1/metrics)
        ├── OTLP/HTTP traces    (tracing spans → tracing-opentelemetry → /v1/traces)
        └── OTLP/HTTP profiles  (pprof CPU → v1development dictionary → /v1/profiles)

doggy-dns-core  : owns ObservabilityConfig + handler spans/outcome
doggy-dns-plugin: owns Middleware trait extension (RequestOutcome)
```

The three supported backends share the same OTLP/HTTP path layout, so a single
producer with a `backend` preset is sufficient:

| Backend | Base endpoint | Paths | Profiles |
|---------|---------------|-------|----------|
| OpenObserve (default) | `http://127.0.0.1:5080/api/default` | `/v1/{logs,metrics,traces,profiles}` | Alpha, supported |
| OTel Collector | `http://127.0.0.1:4318` | `/v1/{logs,metrics,traces,profiles}` | Requires profiles feature gate + pipeline |
| Vector | `http://127.0.0.1:4318` | `/v1/{logs,metrics,traces}` | Not supported |
| Custom | explicit | explicit | explicit |

## Configuration

```toml
[observability]
enabled = false
backend = "openobserve"          # openobserve | otel-collector | vector | custom
endpoint = ""                    # base URL, no /v1 suffix
protocol = "http/protobuf"
service_name = ""                # default: doggy-dns / OTEL_SERVICE_NAME
environment = ""                 # deployment.environment.name
timeout_secs = 10
headers = { "stream-name" = "doggy-dns" }
basic_auth = { username = "root@example.com", password = "${OPENOBSERVE_PASSWORD}" }
resource_attributes = { "service.namespace" = "dns" }

[observability.logs]
enabled = true
level = "warn"
stdout = true
endpoint = ""                    # full URL override

[observability.metrics]
enabled = true
interval_secs = 15
temporality = "cumulative"
endpoint = ""

[observability.traces]
enabled = true
sample_ratio = 0.05
max_queue_size = 8192
max_export_batch_size = 512
scheduled_delay_ms = 2000
endpoint = ""

[observability.profiles]
enabled = false
interval_secs = 60
duration_secs = 10
frequency_hz = 99
endpoint = ""
```

Environment variables:

- `OTEL_EXPORTER_OTLP_ENDPOINT` (base), `OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT` (full URL)
- `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k2=v2`)
- `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`
- `${ENV_VAR}` interpolation inside configured header/auth values

Config wins over environment; environment wins over the backend preset.

## Signals

### Logs

- `OpenTelemetryTracingBridge` layer; trace context attached automatically.
- Filter: configured level, and **exclude `opentelemetry*` targets** to avoid
  recursive export of SDK internal logs.
- `stdout` keeps the existing fmt layer; `middleware.log_format = "json"` is honored.

### Metrics

- `dns.queries` counter: `dns.question.type`, `network.protocol.name`,
  `dns.response.code`, `dns.authority`.
- `dns.query.duration` histogram in ms with DNS-friendly boundaries
  `0.1, 0.25, 0.5, 1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000`.
- `dns.queries.in_flight` up/down counter.
- qname/client IP never become metric attributes (cardinality).

### Traces

- `dns.request` root span (`otel.kind = "server"`), child `dns.resolve`,
  `dns.forward` spans.
- `ParentBased(TraceIdRatioBased(sample_ratio))`.
- Batch queue/batch/delay configurable; defaults sized for high DNS QPS.

### Profiles

- pprof CPU sampling (frequency/window/interval configurable).
- Convert pprof `Report` to OTLP `profiles.v1development`:
  - dictionary table index 0 must be the zero value for every table;
  - `sample_type = cpu/nanoseconds`, value = `count * 1e9 / frequency`;
  - thread name stored in `attribute_table`;
  - resource includes `service.name` (required by OpenObserve).
- POST `application/x-protobuf` to `/v1/profiles` with configured headers/auth.
- Linux/macOS only; Vector is warned as unsupported.

## Reliability / performance

- Export failures never block DNS serving; batch processors drop on queue overflow.
- Profiles default off; traces default 5%; OTLP logs default warn.
- Graceful shutdown flushes providers with `shutdown_timeout`; profile task is aborted.
- Secrets are redacted from Debug output and never logged.

## File changes

- `Cargo.toml` — workspace member + dependency versions
- `crates/doggy-dns-observability/` — new crate
- `crates/doggy-dns-core/src/config.rs` — config + validation
- `crates/doggy-dns-plugin/src/plugin.rs` — `RequestOutcome` + default method
- `crates/doggy-dns-core/src/handler.rs` — spans + outcome
- `crates/doggy-dns-plugin/src/builtin/forward.rs` — `dns.forward` span
- `src/main.rs` — telemetry init/shutdown, metrics middleware
- `config/doggy-dns.toml`, `README.md`, `docs/observability.md`, `AGENTS.md`

## Verification

- Unit: config parsing/validation, endpoint precedence, header/auth/env
  interpolation, profile dictionary conversion.
- Integration: mock OTLP HTTP server asserts the four paths, auth/stream
  headers, protobuf decoding, and `service.name`.
- E2E: doggy-dns → local OpenObserve v1.1.2, query DNS and verify streams.
- `cargo test`, `cargo clippy --all-features --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`.

## Implementation result

- `cargo test --workspace`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, and `cargo fmt --all -- --check` pass.
- E2E against local OpenObserve v1.1.2:
  - logs stream `doggy_dns_final`, traces stream `doggy_dns_final`, metrics
    streams `dns_queries` / `dns_query_duration_*` all contain data;
  - trace spans include `dns.request` (server) and `dns.resolve` children,
    with `client.port` and response-code attributes;
  - every `tracing::error!` event produces a force-sampled `dns.error` child
    span that keeps the original trace ID, independent of `sample_ratio`;
  - profiles are exported (38 KB POST, HTTP 200) when CPU samples exist and are
    stored as a `stream_type=profiles` stream with queryable data on
    OpenObserve v1.1.2;
  - empty profile windows are skipped;
  - SIGTERM during an active profile window shuts down in ~0.08 s.
