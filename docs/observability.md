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

The endpoint is the OpenObserve **base** URL, `http://<host>:5080/api/<org>`;
doggy-dns appends `/v1/<signal>`. Do not put a trailing slash or a `/v1` suffix
in `[observability] endpoint` — use the per-signal `endpoint` for a full URL
(trailing slashes there are normalized).

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

`host.name` defaults to `HOSTNAME`/`COMPUTERNAME`, then the OS hostname, and
`service.instance.id` is `<host.name>-<pid>`; an explicitly configured
`host.name` drives both. Alongside it, doggy-dns publishes
`doggy_dns.ip_addr` (the reachable node address) and its listener ports
(`doggy_dns.dns.port`, `doggy_dns.control.port` whenever the control plane is
enabled, and TLS/HTTPS ports when enabled) so centralized consoles can discover
and join instances from telemetry alone. See
[configuration.md](configuration.md#control-plane).

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

Vector receives logs/metrics/traces (not profiles) and forwards them to
OpenObserve or another backend with its own sinks, so doggy-dns stays a
single-target OTLP producer. Keep `observability.profiles.enabled = false`.

## Metrics

| Metric | Type | Unit | Attributes |
|--------|------|------|------------|
| `dns.queries` | counter | `1` | `dns.question.type`, `network.protocol.name`, `dns.response.code`, `dns.authority` |
| `dns.query.duration` | histogram | `ms` | same |
| `dns.queries.in_flight` | up/down counter | `1` | — |
| `dns.xdp.queries` | counter | `1` | `dns.xdp.result` (`hit`, `negative_hit`, `miss`, `expired`, `drop`) |

Buckets: `0.1, 0.25, 0.5, 1, 2.5, 5, 10, 25, 50, 100, 250, 500, 1000, 2500,
5000` ms. Query names and client IPs are intentionally **not** metric
attributes (cardinality); use logs/traces for those.

The `dns.xdp.queries` instrument is created whenever metrics are enabled, but
only emits data points when `xdp.enabled` is also true on Linux with the `xdp`
feature built in. Kernel-served answers never appear in `dns.queries`, so total
demand is `dns.queries` plus `dns.xdp.queries{result=hit|negative_hit}`; do not
add `miss`/`expired`, which also appear in `dns.queries`. Values come from a
per-CPU map polled once per `interval_secs`, so they lag by up to one poll plus
one export interval but are exact counts, not a sample, unless the program was
reloaded (a counter that went backwards starts fresh, so deltas stay
non-negative while the counts since the last poll are lost). Kernel-side
failure records are logs, not metrics: they arrive under the
`doggy_dns_xdp::kernel` target at `warn`/`error` level. See
[xdp.md](xdp.md) for the kernel side.

## Prometheus pull (control plane)

The control plane can serve the same DNS metrics as Prometheus text at
`GET /metrics`, with no OTLP backend involved:

```toml
[control]
enabled = true
listen_addr = "0.0.0.0"          # Prometheus must be able to dial the address
port = 19090
endpoints = ["healthz", "readyz", "metrics"]
```

Selecting `metrics` installs the DNS metrics middleware and starts the XDP
counter poller even when `[observability] enabled = false`. When OTLP metrics
are also enabled, both readers share one set of instruments, so nothing is
recorded twice on the query path. The endpoint is unauthenticated like the
health probes and its `target_info` carries node identity, so bind it to
loopback or restrict the control port with a NetworkPolicy/firewall when the
node network is not trusted; `/mcp` keeps its Bearer token.

```yaml
scrape_configs:
  - job_name: doggy-dns
    static_configs:
      - targets: ["dns-node-1:19090"]
```

| Metric | Type | Labels |
|--------|------|--------|
| `dns_queries_total` | counter | `dns_question_type`, `network_protocol_name`, `dns_response_code`, `dns_authority` |
| `dns_query_duration_bucket` / `_sum` / `_count` | histogram, values in **ms** | same as `dns_queries_total` |
| `dns_queries_in_flight` | gauge | — |
| `dns_xdp_queries_total` | counter | `dns_xdp_result` (`hit`, `negative_hit`, `miss`, `expired`, `drop`) |
| `target_info` | gauge | resource attributes: `service_name`, `service_version`, `service_instance_id`, `host_name`, `doggy_dns_ip_addr`, listener ports, ... |

Prometheus-specific naming: units are not folded into the name (OTel unit
`1` would otherwise produce a misleading `_ratio` suffix), so the histogram
stays `dns_query_duration_*` with milliseconds as its value, and the OTel
instrumentation-scope labels are omitted. `dns.xdp.queries` is still polled
once per `[observability.metrics].interval_secs`, so kernel counters lag by up
to one poll plus one scrape interval. Runtime JSON signals (Nacos cache
occupancy, push freshness, XDP attach state) remain on `/readyz`, not
`/metrics`.

## Traces

| Span | Attributes |
|------|------------|
| `dns.request` | `request_id`, `dns.question.name`, `dns.question.type`, `network.protocol.name`, `client.address`, `client.port`, `dns.response.code`, `dns.authority`, `dns.answers`, `dns.duration_ms`, `otel.kind=server` |
| `dns.resolve` | `dns.question.name`, `dns.question.type`, `dns.authority`, `dns.answers` |
| `dns.forward` | `dns.question.name`, `dns.question.type`, `dns.upstream.count`, `dns.answers`, `otel.kind=client` |
| `dns.error` | `error.message`, `error.target`, `code.filepath`, `code.lineno`, `otel.status_code=ERROR`, `doggy.force_sample=true` |

Sampling is parent-based trace-ID ratio for normal traffic.

**Every `tracing::error!` event is force-captured** as a dedicated `dns.error`
span (attributes in the table above), regardless of `sample_ratio` — even at
`0`. It is a child of the current request span, so it keeps the original
`trace_id` even when head sampling dropped the root; outside any span it becomes
a root span. OTel SDK internal ERROR logs are excluded to avoid an export
feedback loop.

> This is driven by `tracing::error!`, not by DNS response codes. Authority
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
- The Collector needs the profiles feature gate and pipeline (see above);
  Vector cannot receive profiles at all.
- Release builds should include debug symbols for useful function names
  (`[profile.release] debug = 1`).
- Collection windows with no CPU samples are skipped (no empty profile is sent),
  which is why the signal is off by default along with its sampling overhead.

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
