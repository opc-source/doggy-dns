# doggy-dns-control

Node-side service control plane for doggy-dns: one HTTP listener exposing the
read-only MCP (Model Context Protocol) introspection endpoint, plain health
probes backed by live runtime signals, and an opt-in Prometheus metrics
endpoint.

## Overview

This crate provides the control plane listener. The MCP protocol side serves
read-only tools over Streamable HTTP; the health endpoints report live runtime
signals sampled at request time. It is designed for integration with the
doggy-dns-console project and other AI-powered management tools, and as a
Kubernetes probe target.

**Key properties:**

- **Disabled by default** — set `[control].enabled = true` to activate.
- **Endpoint selection** — `control.endpoints` decides which routes exist
  (`mcp`, `healthz`, `readyz`, `metrics`); everything else 404s.
- **Prometheus pull** — `/metrics` serves the DNS request metrics as text,
  independent of OTLP push; see [Metrics](#metrics).
- **Read-only** — all four tools carry `readOnlyHint = true`, `destructiveHint = false`.
- **Non-fatal** — if the control plane port is already in use, the DNS server continues running.
- **Config redaction** — `get_config` recursively replaces sensitive values (`password`, `token`, `secret`, `authorization`, `api_key`, `apikey`, `passwd`) with `"***"`.
- **Optional Bearer auth** — set `[control].token` to require `Authorization: Bearer <token>` on `/mcp`; health endpoints never require it.
- **Discovery** — the node publishes `doggy_dns.ip_addr` and `doggy_dns.control.port` OTel resource attributes so doggy-dns-console can join `http://{ip_addr}:{control_port}/mcp` and discover instances without static configuration.

## Configuration

```toml
[control]
enabled = true
listen_addr = "127.0.0.1"
port = 19090
token = "change-me"                    # omit for no auth; guards /mcp only
allowed_hosts = ["dns-node-1", "10.0.0.5:19090"]
endpoints = ["mcp", "healthz", "readyz", "metrics"]
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable the control plane listener |
| `listen_addr` | `"127.0.0.1"` | Bind address; IPv4 or IPv6 literal (`[::1]` also accepted) |
| `port` | `19090` | Bind port |
| `token` | `None` | Bearer token for `/mcp` |
| `allowed_hosts` | `[]` | Allowed `Host` header values for DNS rebinding protection (`/mcp` only) |
| `endpoints` | `["mcp", "healthz", "readyz"]` | Which routes the listener serves; `metrics` is opt-in |

## Endpoints

| Route | Method | Auth | Purpose |
|-------|--------|------|---------|
| `/mcp` | POST (JSON-RPC 2.0, optional SSE) | Bearer token when set | MCP tools |
| `/healthz` | GET | none | Liveness; `{"status":"ok"}`, 200 while the process is up |
| `/readyz` | GET | none | Live runtime signals report; always 200 — a report, not a verdict |
| `/metrics` | GET | none | Prometheus text exposition of the DNS request metrics |

The MCP server listens on `http://<listen_addr>:<port>/mcp` using the
**Streamable HTTP** transport.

## Metrics

`/metrics` is opt-in (`endpoints` must contain `metrics`) and serves
`text/plain; version=0.0.4`. Selecting it collects DNS metrics even when
`[observability] enabled = false`; when OTLP metrics are also enabled, both
readers share one set of instruments. `listen_addr` defaults to loopback, so
set it to an address Prometheus can dial when scraping from another host.

| Metric | Type | Labels |
|--------|------|--------|
| `dns_queries_total` | counter | `dns_question_type`, `network_protocol_name`, `dns_response_code`, `dns_authority` |
| `dns_query_duration_bucket` / `_sum` / `_count` | histogram, values in **ms** | same as above |
| `dns_queries_in_flight` | gauge | — |
| `dns_xdp_queries_total` | counter | `dns_xdp_result` |
| `target_info` | gauge | `service_name`, `service_version`, `service_instance_id`, `host_name`, `doggy_dns_ip_addr`, listener ports, ... |

Units are not folded into the names (OTel unit `1` would otherwise add a
misleading `_ratio` suffix), and the instrumentation-scope labels are omitted.
Runtime JSON signals stay on `/readyz`. See
[docs/observability.md](../../docs/observability.md#prometheus-pull-control-plane)
for a scrape config and the XDP polling caveat.

## Tools

### `get_status`

Returns the instance's identity, version, uptime, and enabled protocols.

**Input:** `{}` (no arguments)

**Output:**

```json
{
  "instanceId": "dns-node-1-4242",
  "host": "dns-node-1",
  "version": "0.1.0",
  "pid": 4242,
  "uptimeSeconds": 3600,
  "startedAt": "2026-10-26T10:00:00+00:00",
  "listenAddr": "0.0.0.0:15353",
  "protocols": ["udp", "tcp", "dot", "doh"],
  "status": "running"
}
```

### `list_plugins`

Returns all configured DNS authority plugins in evaluation order.

**Input:** `{}` (no arguments)

**Output:**

```json
{
  "plugins": [
    {
      "order": 0,
      "kind": "nacos",
      "enabled": true,
      "details": {
        "serverAddr": "127.0.0.1:8848",
        "namespace": "public",
        "group": "DEFAULT_GROUP",
        "dnsZone": "nacos.local",
        "ttl": 6
      }
    },
    {
      "order": 1,
      "kind": "forward",
      "enabled": true,
      "details": {
        "upstream": ["8.8.8.8:53", "1.1.1.1:53"],
        "cacheSize": 1024,
        "minTtl": 30,
        "maxTtl": 600
      }
    }
  ]
}
```

### `get_config`

Returns the doggy-dns TOML configuration with sensitive values redacted.
The raw file content is captured once by `load_config` at startup and
redacted when the MCP state is built; the tool never touches the
filesystem per call, so comments, ordering and unknown keys are
preserved exactly as written.

**Input:** `{}` (no arguments)

**Output:**

```json
{
  "format": "toml",
  "content": "[server]\nlisten_addr = \"0.0.0.0\"\nport = 15353\n...",
  "redactedKeys": ["observability.basic_auth.password"]
}
```

If the captured content cannot be parsed as TOML (defensive; startup
validation normally prevents this):

```json
{
  "error": {
    "code": "config_parse_error",
    "message": "failed to parse config: ..."
  }
}
```

### `get_runtime_status`

Returns live runtime signals sampled at call time — the same payload
`/readyz` serves over GET. One signal per registered source:

**Input:** `{}` (no arguments)

**Output:**

```json
{
  "signals": {
    "nacos.nacos.local": {
      "cachedServices": 42,
      "lastPushSecondsAgo": 3
    },
    "xdp": {
      "attached": true
    }
  }
}
```

`lastPushSecondsAgo` is `null` when no push has ever arrived for that zone.
`xdp.attached` is only reported by builds with the fast path wired in; the
flag is fixed after startup because dropping the manager detaches the
program.

## Authentication

When `[control].token` is set, requests to `/mcp` must include:

```
Authorization: Bearer <token>
```

`/mcp` requests without a valid token receive `401 Unauthorized`.
`/healthz`, `/readyz` and `/metrics` never require auth — probes and scrapers
must not need secrets.

## Contract Stability

The following JSON field names are part of the stable contract and must not be renamed:

- `get_status` → `instanceId`, `host`, `version`, `pid`, `uptimeSeconds`, `startedAt`, `listenAddr`, `protocols`, `status`
- `list_plugins` → `plugins[].order`, `kind`, `enabled`, `details`
- `get_config` → `format`, `content`, `redactedKeys`
- `get_runtime_status` / `GET /readyz` → `signals`
- `GET /healthz` → `status`
- `GET /metrics` → Prometheus metric names and labels listed in [Metrics](#metrics)

## Smoke Test

```bash
# Enable the control plane in config/doggy-dns.toml ([control] enabled = true), then:
cargo run -- config/doggy-dns.toml

# Liveness and readiness probes (no auth, always 200)
curl -s http://127.0.0.1:19090/healthz
curl -s http://127.0.0.1:19090/readyz

# Prometheus metrics (requires "metrics" in [control].endpoints)
curl -s http://127.0.0.1:19090/metrics

# Initialize MCP session
curl -s http://127.0.0.1:19090/mcp \
  -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'

# List tools
curl -s http://127.0.0.1:19090/mcp \
  -H 'content-type: application/json' \
  -H 'accept: application/json, text/event-stream' \
  -H 'mcp-session-id: <session-id-from-above>' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
```

## Read-Only Guarantee

This server provides **no write operations**. There are no tools, resources, or prompts that modify configuration, clear caches, restart services, or perform any destructive action. All tool annotations declare:

- `readOnlyHint = true`
- `destructiveHint = false`
- `idempotentHint = true`
- `openWorldHint = false`
