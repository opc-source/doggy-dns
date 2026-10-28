# Configuration Reference

TOML-based. Default path: `config/doggy-dns.toml` (or pass as CLI arg).
Part of [README](../README.md); see also [observability.md](observability.md),
[architecture.md](architecture.md), and [xdp.md](xdp.md).

## Full reference

```toml
[server]
listen_addr = "0.0.0.0"
port = 15353
tcp_timeout = 10          # seconds, default 10
shutdown_timeout = 5      # seconds, default 5
worker_threads = 1        # default 1 (suitable for DaemonSet, one per node)

# Optional: DNS over TLS
#[server.tls]
#enabled = true
#port = 853
#cert_path = "/path/to/cert.pem"
#key_path = "/path/to/key.pem"

# Optional: DNS over HTTPS
#[server.https]
#enabled = true
#port = 443
#cert_path = "/path/to/cert.pem"
#key_path = "/path/to/key.pem"

[middleware]
logging = true

# Optional OTLP export (disabled by default). Per-signal fields, backend
# presets and endpoint precedence: docs/observability.md
[observability]
enabled = false
backend = "openobserve"                  # openobserve | otel-collector | vector | custom
endpoint = "http://127.0.0.1:5080/api/default"
service_name = "doggy-dns"
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
sample_ratio = 0.05   # normal traffic; tracing::error! spans are always captured

[observability.profiles]
enabled = false       # CPU only, Linux/macOS, OTLP Profiles Alpha

# Plugins are evaluated in order. First non-Skip result wins.
[[plugins]]
kind = "nacos"
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "native"
enabled = true
cache_size = 4096         # default 4096
min_ttl = 10              # seconds, default 10
max_ttl = 300             # seconds, default 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53", "1.1.1.1:53"]   # IP or IP:port; port defaults to 53
cache_size = 1024
min_ttl = 30
max_ttl = 600

# Optional: hot-reload config from Nacos config service
[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DOGGY_DNS"
data_id = "doggy-dns.toml"

# Optional: node-side service control plane (one HTTP listener).
# `endpoints` selects what is exposed: "mcp" (read-only JSON-RPC tools at
# /mcp), "healthz" (liveness probe), "readyz" (live runtime signals report).
[control]
enabled = true
listen_addr = "127.0.0.1"
port = 19090
token = "change-me"                    # optional Bearer auth; guards /mcp only
endpoints = ["mcp", "healthz", "readyz"]

# Optional: XDP kernel fast path (Linux, requires `--features xdp` build).
# Usage, semantics and limits: docs/xdp.md
[xdp]
enabled = false
interface = ""              # empty = XDP_INTERFACE env, else default-route interface
cache_size = 65536          # kernel cache entries; applied when the map is created
negative_ttl = 30           # seconds, default 30
attach_mode = "auto"        # "auto" attaches at startup, "manual" loads only
pin_path = "/sys/fs/bpf/doggy-dns/dns_cache"
```

`listen_addr` (`[server]` and `[control]`) takes an IPv4 or IPv6 literal. IPv6 may
be written bare (`::1`, `::`) or bracketed (`[::1]`); `::` listens on all
interfaces. TLS and HTTPS reuse `[server].listen_addr` with their own port.

If no `[[plugins]]` are configured, defaults to forwarding to `8.8.8.8` and
`1.1.1.1`.

## Endpoint formats

The base endpoint is `<host>/api/<org>` for OpenObserve and `http://host:4318`
for Collector/Vector; doggy-dns appends `/v1/<signal>`.

## Environment expansion

Every string value may reference the environment as `${NAME}`. References are
resolved when the config is loaded; a missing variable aborts startup and the
error names the offending key (e.g. `observability.basic_auth.password`).
Expansion happens before deserialisation, so numeric fields cannot be sourced
from the environment (`port = "${PORT}"` fails to parse). The MCP `get_config`
tool returns the file as written, so expanded secrets never leave the process.

## Validation

Startup validation (`validate_config()`) checks: `worker_threads > 0`; TLS and
HTTPS cert files exist when enabled; `remote_config` fields non-empty when
enabled; plugin rules — at least one enabled plugin when `[[plugins]]` is
present, Nacos `server_addr`/`namespace`/`group`/`dns_zone` present with a
syntactically valid and unique zone, `min_ttl <= max_ttl` for native/forward,
and forward `upstream` entries parse as IP or IP:port; observability signal
ranges, profile duration/frequency, log level and endpoint format;
`xdp.cache_size > 0`, `xdp.negative_ttl > 0` and `xdp.attach_mode` is `auto` or
`manual` when the fast path is enabled; `control.listen_addr` is a valid IP
literal, `control.endpoints` is non-empty and contains only `mcp`, `healthz`
and `readyz` (no duplicates) when the control plane is enabled.

## Control plane

One HTTP listener whose exposed surface is chosen by `control.endpoints`;
disabled by default. `mcp` nests the read-only JSON-RPC tools at `/mcp` over
Streamable HTTP — `get_status` (instance identity/uptime), `list_plugins`
(authority chain), `get_config` (TOML with secrets redacted) and
`get_runtime_status` (live signals sampled at call time); the Bearer token
guards this endpoint only. `healthz` is a trivial liveness probe and `readyz`
reports live runtime signals (per-zone Nacos cache occupancy and push
freshness, XDP attach state); both are plain GET, never require auth and
always answer 200 — `readyz` is a report, not a verdict, so deciding what
counts as "not ready" stays with the operator or doggy-dns-console. Full
contract: [crates/doggy-dns-mcp/README.md](../crates/doggy-dns-mcp/README.md).
