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

# Optional OTLP export (disabled by default).
# Full signal reference, backend presets, Collector/Vector examples:
# docs/observability.md
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
enabled = false
interval_secs = 60
duration_secs = 10
frequency_hz = 99

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

# Optional: read-only MCP introspection server
[mcp]
enabled = true
listen_addr = "127.0.0.1"
port = 19090
token = "change-me"                    # optional Bearer auth

# Optional: XDP kernel fast path (Linux, requires `--features xdp` build).
# Usage, semantics and limits: docs/xdp.md
[xdp]
enabled = false
interface = ""              # empty = XDP_INTERFACE env, else default-route interface
cache_size = 65536          # informational; the BPF map size is fixed by the program
negative_ttl = 30           # seconds, default 30
attach_mode = "auto"        # "auto" attaches at startup, "manual" loads only
pin_path = "/sys/fs/bpf/doggy-dns/dns_cache"
```

If no `[[plugins]]` are configured, defaults to forwarding to `8.8.8.8` and
`1.1.1.1`.

## Endpoint formats

The base endpoint is `<host>/api/<org>` for OpenObserve and `http://host:4318`
for Collector/Vector; doggy-dns appends `/v1/<signal>`.

## Validation

Startup validation (`validate_config()`) checks: `worker_threads > 0`; TLS and
HTTPS cert files exist when enabled; `remote_config` fields non-empty when
enabled; observability signal ranges, profile duration/frequency, log level and
endpoint format; `xdp.cache_size > 0`, `xdp.negative_ttl > 0` and
`xdp.attach_mode` is `auto` or `manual` when the fast path is enabled.

## MCP server

Serves read-only tools over Streamable HTTP at `/mcp`; disabled by default.
Three tools: `get_status` (instance identity/uptime), `list_plugins` (authority
chain), `get_config` (TOML with secrets redacted). Full contract:
[crates/doggy-dns-mcp/README.md](../crates/doggy-dns-mcp/README.md).
