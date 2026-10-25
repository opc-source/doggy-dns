# Observability (OTLP → OpenObserve / OTel Collector / Vector)

doggy-dns exports logs, metrics, traces, and CPU profiles over **OTLP/HTTP
(protobuf)**. OpenObserve is the default backend; OTel Collector and Vector use
the same `/v1/<signal>` paths, so only the endpoint (and optionally
authentication) changes.

> Export is disabled by default. Enable it with `[observability] enabled = true`.

## Signals

| Signal | Endpoint | Default | Notes |
|--------|----------|---------|-------|
| Logs | `/v1/logs` | enabled, level `warn` | stdout logging stays independent |
| Metrics | `/v1/metrics` | enabled, 15s interval | DNS-aware histogram buckets |
| Traces | `/v1/traces` | enabled, 5% sampling | `dns.request` / `dns.resolve` / `dns.forward` spans |
| Profiles | `/v1/profiles` | disabled | CPU only, Linux/macOS, OTLP Profiles Alpha |

## Quick start: OpenObserve

Start OpenObserve (or use an existing deployment):

```bash
podman run -d --name openobserve -p 5080:5080 \
  -e ZO_ROOT_USER_EMAIL=root@example.com \
  -e ZO_ROOT_USER_PASSWORD='Complexpass#123' \
  public.ecr.aws/zinclabs/openobserve:latest
```

Configure doggy-dns (`config/doggy-dns.toml`):

```toml
[observability]
enabled = true
backend = "openobserve"
endpoint = "http://127.0.0.1:5080/api/default"
service_name = "doggy-dns"
environment = "production"
headers = { "stream-name" = "doggy-dns" }
basic_auth = { username = "root@example.com", password = "${OPENOBSERVE_PASSWORD}" }

[observability.logs]
enabled = true
level = "warn"
stdout = true

[observability.metrics]
enabled = true
interval_secs = 15

[observability.traces]
enabled = true
sample_ratio = 0.05            # tracing::error! spans are always captured

[observability.profiles]
enabled = false
```

Export the password (never commit it):

```bash
export OPENOBSERVE_PASSWORD='Complexpass#123'
cargo run --release -- config/doggy-dns.toml
```

In OpenObserve, open **Logs / Metrics / Traces / Profiles** and select the
`doggy-dns` stream (or `default` if `stream-name` is not set). OpenObserve
sanitizes stream names: a `stream-name` of `doggy-dns` appears as
`doggy_dns`. Metrics are flattened into one stream per metric
(`dns_queries`, `dns_query_duration_bucket`, `dns_query_duration_count`,
`dns_query_duration_sum`, ...).

The endpoint is the OpenObserve **base** URL:

```
http://<host>:5080/api/<org>
```

doggy-dns appends `/v1/logs`, `/v1/metrics`, `/v1/traces`, or `/v1/profiles`.
Do not include a trailing slash or a `/v1` or `/v1/<signal>` suffix in
`endpoint`; use the per-signal `endpoint` for a full URL override. Trailing
slashes on full per-signal URLs are normalized.

## Configuration reference

```toml
[observability]
enabled = false
backend = "openobserve"        # openobserve | otel-collector | vector | custom
endpoint = ""                  # empty: env var, then backend default
protocol = "http/protobuf"     # only HTTP/protobuf; grpc/http-json env vars are ignored with a warning
service_name = ""              # default doggy-dns / OTEL_SERVICE_NAME
environment = ""               # deployment.environment.name
timeout_secs = 10
headers = {}                   # values support ${ENV_VAR}
basic_auth = { username = "", password = "" }
resource_attributes = {}

[observability.logs]
enabled = true
level = "warn"                 # trace | debug | info | warn | error
stdout = true
endpoint = ""                  # full URL override

[observability.metrics]
enabled = true
interval_secs = 15
temporality = "cumulative"     # cumulative | delta
endpoint = ""

[observability.traces]
enabled = true
sample_ratio = 0.05            # tracing::error! spans are always captured
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

### Backend presets

| `backend` | Default endpoint | Profiles |
|-----------|------------------|----------|
| `openobserve` | `http://127.0.0.1:5080/api/default` | supported (Alpha) |
| `otel-collector` | `http://127.0.0.1:4318` | requires feature gate + pipeline |
| `vector` | `http://127.0.0.1:4318` | **not supported** |
| `custom` | must set `endpoint` | depends on the receiver |

Presets only supply defaults and startup warnings; the endpoint path logic is
always `<base>/v1/<signal>`.

### Endpoint precedence

1. per-signal `endpoint` (full URL)
2. `[observability] endpoint` (base URL)
3. `OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT` (full URL)
4. `OTEL_EXPORTER_OTLP_ENDPOINT` (base URL)
5. backend preset default

Headers merge from `OTEL_EXPORTER_OTLP_HEADERS`, then config headers (config
wins). Values from `OTEL_EXPORTER_OTLP_HEADERS` and
`OTEL_RESOURCE_ATTRIBUTES` are percent-decoded (`%20`, `%2C`, ...).
`basic_auth` adds `Authorization: Basic ...` only when no `Authorization`
header is configured.

Resource attributes merge from `OTEL_RESOURCE_ATTRIBUTES`, then
`resource_attributes` (config wins). `service.name` is resolved from
`service_name` → `OTEL_SERVICE_NAME` → resource attributes → `doggy-dns`.

## OTel Collector

Point doggy-dns at the Collector's OTLP/HTTP receiver:

```toml
[observability]
enabled = true
backend = "otel-collector"
endpoint = "http://127.0.0.1:4318"
```

Collector pipeline (OpenObserve exporter):

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318

exporters:
  otlphttp/openobserve:
    endpoint: http://openobserve:5080/api/default
    headers:
      Authorization: Basic <base64(email:password)>
      stream-name: doggy-dns

service:
  pipelines:
    logs:
      receivers: [otlp]
      exporters: [otlphttp/openobserve]
    metrics:
      receivers: [otlp]
      exporters: [otlphttp/openobserve]
    traces:
      receivers: [otlp]
      exporters: [otlphttp/openobserve]
```

For profiles, start the Collector with
`--feature-gates=+service.profilesSupport`, add `profiles_endpoint` to the
exporter, and add a `profiles` pipeline (OpenObserve's OTLP profiles support is
Alpha). If the Collector is not configured for profiles, leave
`observability.profiles.enabled = false`.

## Vector

Vector's `opentelemetry` source accepts OTLP/HTTP and OTLP/gRPC. doggy-dns uses
HTTP:

```toml
[observability]
enabled = true
backend = "vector"
endpoint = "http://127.0.0.1:4318"
```

```yaml
sources:
  doggy_dns:
    type: opentelemetry
    http:
      address: 0.0.0.0:4318
    decoding:
      codec: otlp
```

Vector outputs logs/metrics/traces, **not profiles**; keep profiles disabled.
Forward the three signals from Vector to OpenObserve (or another backend) using
Vector's own sinks. This keeps doggy-dns a single-target OTLP producer.

## Metrics

| Metric | Type | Unit | Attributes |
|--------|------|------|------------|
| `dns.queries` | counter | `1` | `dns.question.type`, `network.protocol.name`, `dns.response.code`, `dns.authority` |
| `dns.query.duration` | histogram | `ms` | same |
| `dns.queries.in_flight` | up/down counter | `1` | — |

Buckets: `0.1, 0.25, 0.5, 1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500,
5000` ms. Query names and client IPs are intentionally **not** metric
attributes (cardinality); use logs/traces for those.

## Traces

| Span | Attributes |
|------|------------|
| `dns.request` | `request_id`, `dns.question.name`, `dns.question.type`, `network.protocol.name`, `client.address`, `client.port`, `dns.response.code`, `dns.authority`, `dns.answers`, `dns.duration_ms`, `otel.kind=server` |
| `dns.resolve` | `dns.question.name`, `dns.question.type`, `dns.authority`, `dns.answers` |
| `dns.forward` | `dns.question.name`, `dns.question.type`, `dns.upstream.count`, `dns.answers`, `otel.kind=client` |
| `dns.error` | `error.message`, `error.target`, `code.filepath`, `code.lineno`, `otel.status_code=ERROR`, `doggy.force_sample=true` |

Sampling is parent-based trace-ID ratio for normal traffic.

**Every `tracing::error!` event is force-captured.** The observability layer
converts it into a dedicated `dns.error` span regardless of `sample_ratio`,
including `sample_ratio = 0`. The span is a **child of the current request
span**, so it keeps the original `trace_id` even when the request root was
dropped by head sampling (the root may be absent, leaving an orphan child in
the same trace). If the ERROR event occurs outside any span, `dns.error`
becomes a root span. It carries:

- `otel.status_code = ERROR`, `otel.kind = server`
- `error.message`, `error.target`
- `code.filepath`, `code.lineno`
- `doggy.force_sample = true`

Normal request spans stay ratio-sampled; only ERROR events get this extra
forced span. OTel SDK internal ERROR logs are excluded to avoid an export
feedback loop.

> This is based on `tracing::error!` events, not DNS response codes. Authority
> paths that log at `WARN` do not trigger it; the forward authority logs
> upstream lookup failures at `ERROR`, so those do.

## Profiles

Profiles are CPU-only, collected with `pprof` on Linux/macOS, and exported to
`/v1/profiles` using the OTLP `profiles.v1development` dictionary layout.
`sample_type` is `cpu/nanoseconds`; values are `sample_count * period`.

Requirements and caveats:

- OpenObserve profiles support is **Alpha**. Verified on the OpenObserve
  **v1.1.2** open-source build: an active CPU profile is stored as a
  `stream_type=profiles` stream and is queryable. List it with
  `GET /api/<org>/streams?type=profiles&offset=0&limit=10`; the stream may not
  appear until the first non-empty profile is flushed.
- OTel Collector requires `--feature-gates=+service.profilesSupport` and a
  profiles pipeline.
- Vector does not support profiles.
- Release builds should include debug symbols for useful function names
  (`[profile.release] debug = 1`).
- Profile collection is disabled by default (sampling overhead + Alpha status).
- Collection windows with no CPU samples are skipped (no empty profile is sent).

## Operational notes

- Export never blocks DNS serving; OTel batch processors drop on queue
  overflow. Increase `max_queue_size` or lower `sample_ratio` under high QPS.
- OTLP logs default to `warn` because the existing per-query `info` logs are
  very chatty at DNS QPS. Set `level = "info"` if you need them, and consider
  `middleware.logging = false`.
- On shutdown, providers are flushed with `timeout_secs`; the profile task is
  aborted.
- Secrets in `basic_auth` are redacted from `Debug`; headers are never logged.
  Prefer `${ENV_VAR}` for passwords.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `401 Unauthorized` | Check `basic_auth` / `Authorization` header; OpenObserve needs Basic auth |
| `404` on all signals | Endpoint must be the base (`.../api/default`), not a full `/v1/...` URL |
| No profiles in OpenObserve | Generate CPU load (empty collection windows are skipped), enable `profiles.enabled`, then check `GET /api/<org>/streams?type=profiles&offset=0&limit=10` |
| Traces missing under load | Lower `sample_ratio` or increase `max_queue_size`; `tracing::error!` spans are force-sampled and should still appear |
| Logs missing | OTLP log level defaults to `warn`; set `observability.logs.level` |
| Collector rejects profiles | Start with `--feature-gates=+service.profilesSupport` and add a profiles pipeline |
