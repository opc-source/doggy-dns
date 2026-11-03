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
#reuseport_listen_num = 1 # SO_REUSEPORT UDP sockets; default 1 (single socket).
                          # Linux only; recommended: physical core count.
#cpu_affinity = [0, 2, 4, 6] # one core per worker thread; see "CPU affinity".
rotate_answers = true     # default true: cycle multi-record answers per response

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

# Optional: remote config source; a push here triggers a hot reload.
# Which settings a reload may change and which it refuses: see "Hot reload".
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
                            # the bpffs file is <pin_path>_v1; a restart reuses
                            # that map, so the path must not contain a dot
```

`listen_addr` (`[server]` and `[control]`) takes an IPv4 or IPv6 literal. IPv6 may
be written bare (`::1`, `::`) or bracketed (`[::1]`); `::` listens on all
interfaces. TLS and HTTPS reuse `[server].listen_addr` with their own port.

If no `[[plugins]]` are configured, defaults to forwarding to `8.8.8.8` and
`1.1.1.1`.

## CPU affinity

`cpu_affinity` gives each tokio worker thread its own core, so a worker keeps that
core's caches for the process lifetime instead of being re-placed wherever the
scheduler finds room. The list must be as long as `worker_threads` with unique core
ids; threads claim entries in start order, so the core *set* is fixed while which
worker lands on which core is not.

```text
  cpu_affinity = [0, 2, 4, 6], worker_threads = 4

  kernel scheduler                userspace: doggy-dns process
  ------------------              ------------------------------------
  core 0  ─────────────────────▶  worker #0  (first thread to start)
  core 1   (no worker here)       free for the load generator / OS
  core 2  ─────────────────────▶  worker #1
  core 3   (no worker here)       free for the load generator / OS
  core 4  ─────────────────────▶  worker #2
  core 5   (no worker here)       free for the load generator / OS
  core 6  ─────────────────────▶  worker #3
  core 7   (no worker here)       free for the load generator / OS
```

On a 4-core / 8-vCPU Linux node with the load generator pinned to the *other*
cores, that is +3.8% ~ +4.6% QPS at `worker_threads = 4`; sharing a core with a
competitor costs 21% ~ 30% — far more than the gain, which is why
[cpu-affinity-report-2026-11-02.md](cpu-affinity-report-2026-11-02.md) insists on
pinning the client too.

Two caveats: a core id is a hard mask on Linux/Android, Windows and the BSDs but
only an L2-placement *hint* on Apple platforms (the Apple Silicon host checked here
rejected the call outright, kern 46), and nothing in the log says whether a pin
landed — confirm on the target with `taskset -p`/`ps -o psr`. Blocking threads,
such as CPU profile collection, draw from the same list round-robin and can share a
worker's core.

## Answer rotation

`rotate_answers` (default `true`) cycles the answers of a response carrying
more than one record: the *k*-th such query, counted across the whole process,
starts at the *k*-th instance. A client that follows the usual convention of
using the first record then spreads across the service instead of pinning to
whatever the registry happened to report first. The cursor only advances on
responses with more than one record, and it is shared by every name and query
type. One cursor for the whole process, rather than one per name, costs no
memory and no eviction policy; concurrent traffic on other names only makes a
name's step jump further ahead, which spreads no worse. Set it to `false` to
get the authority's own order on every query — useful when a client or a test
needs a stable answer set.

Rotation happens in the handler, so `nacos`, `native` and `forward` all get it,
and it never touches a single-record answer, a `NODATA`, an `NXDOMAIN` or the
apex `SOA`. With the XDP fast path enabled the kernel answers a cache hit
without consulting userspace again, so a hit replays the order that was captured
when the entry was written until its TTL expires; startup logs one `warn` when
both settings are on. See [xdp.md](xdp.md#limitations--notes).

## Hot reload

Two triggers ask a running process to rebuild its query path: `SIGHUP`, which
re-reads the config file the process was started with, and — when
`[remote_config] enabled = true` — a push from the Nacos config service. Both end
in the same place: parse, expand `${NAME}`, validate, compare, rebuild, swap. A
pushed config therefore has to satisfy every rule in [Validation](#validation),
and an invalid or unparseable one is dropped with the previous path still
serving.

What a reload may change is an allow-list, because everything else was consumed
before any query could be answered: the sockets and TLS listeners are bound once,
the tokio runtime (worker count, CPU pinning) exists before the config is read a
second time, the tracing subscriber is installed once for the process, the XDP
program attaches at start-up, and `[remote_config]` is the source of the change
rather than something it can carry.

| Applied in place | Requires a restart |
| --- | --- |
| `[[plugins]]` — add, drop, disable, reorder, `dns_zone`, `ttl`, `upstream`, cache sizes and TTL bounds | `server.listen_addr`, `server.port`, `server.tcp_timeout`, `server.shutdown_timeout`, `server.worker_threads`, `server.reuseport_listen_num`, `server.cpu_affinity`, `[server.tls]`, `[server.https]` |
| `server.rotate_answers` | `middleware.log_format` |
| `middleware.logging` | `[remote_config]`, `[observability]`, `[xdp]`, `[control]` |

A reload is all-or-nothing. If a candidate changes anything in the right-hand
column, the whole thing is refused — the reloadable half is not applied either —
and the log names every offending path
(`config reload rejected: restart required; conflicts=server.port`) so the
operator sees the line they actually changed. Half-applied configs are the
failure mode this rule exists to prevent: answers that quietly disagree with the
config file on disk.

The swap itself is atomic. A query in flight finishes on the chain it started
with, so a reload never changes the rules under a request mid-resolution. Reload
requests are coalesced over a 500 ms window and the newest wins, which folds a
burst of `SIGHUP`s — or the several callbacks Nacos delivers for one multi-change
edit — into a single rebuild.

**Nacos connections survive a reload.** A watcher is keyed by its connection
identity (`server_addr`, `namespace`, `group`), so re-publishing a zone under a
new name or changing a TTL reuses the existing gRPC connection and the instances
already pushed into its cache. Only a config that changes one of those three
fields opens a new connection. A watcher whose server no plugin asks for is kept
rather than released — the Nacos SDK exposes no way to close a naming connection,
so orphans are dropped by a restart.

**What a reload cannot pick up.** A reload does not re-subscribe an existing
watcher, so services registered in Nacos after start-up only reach a *pooled*
connection's cache if the server pushed them; a fresh subscription needs a restart
(or a change to a connection field, which opens a new watcher). Control-plane
live signals (`/readyz`) and the MCP plugin list are built at start-up too:
per-zone signals for a zone added by a reload, and any `[control]` change, wait
for the restart.

**A push is in-memory only.** The pushed content becomes what the process
answers from, but the local file keeps its previous content and is what the next
start reads. Treat the remote config service as the source of truth once it is
enabled, or write the pushed content back to the file before restarting.

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

Startup validation (`validate_config()`) checks: `worker_threads > 0`;
`reuseport_listen_num >= 1`, and only above 1 on Linux; `cpu_affinity` non-empty
when given, its length equal to `worker_threads`, its core ids unique (one outside
the available set aborts startup when the runtime is built); TLS and HTTPS cert
files exist when enabled; `remote_config` fields non-empty when
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
contract: [crates/doggy-dns-control/README.md](../crates/doggy-dns-control/README.md).

When observability is enabled, the node publishes `doggy_dns.ip_addr` — a
concrete bind address as-is, loopback for a loopback bind, or the egress
interface IP for a wildcard bind — plus `doggy_dns.dns.port` and
`doggy_dns.control.port` whenever the control plane is enabled, regardless of
which endpoints it serves (TLS/HTTPS ports are included when those listeners
are enabled). doggy-dns-console joins
`http://{ip_addr}:{control_port}/mcp`, so no static endpoint list is needed.
If the console reaches nodes through a different address (NAT, reverse proxy),
configure `MCP_ENDPOINTS` there.
