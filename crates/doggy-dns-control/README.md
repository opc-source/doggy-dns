# doggy-dns-control

Node-side service control plane for doggy-dns: one HTTP listener exposing the
MCP (Model Context Protocol) endpoint — introspection tools plus one runtime
control — and plain health probes backed by live runtime signals.

## Overview

This crate provides the control plane listener. The MCP protocol side serves
tools over Streamable HTTP; all but `set_log_level` are read-only. The health
endpoints report live runtime signals sampled at request time. It is designed
for integration with the doggy-dns-console project and other AI-powered
management tools, and as a Kubernetes probe target.

**Key properties:**

- **Disabled by default** — set `[control].enabled = true` to activate.
- **Endpoint selection** — `control.endpoints` decides which routes exist
  (`mcp`, `healthz`, `readyz`); everything else 404s.
- **One runtime control** — `set_log_level` retargets the log filters
  (`readOnlyHint = false`, non-destructive, idempotent); the other four tools
  are read-only, and the current filters ride along in `get_runtime_status`.
- **Non-fatal** — if the control plane port is already in use, the DNS server continues running and the bind is retried every 10s until the port frees (see `BIND_RETRY_INTERVAL`). The control port is deliberately *not* in the `SO_REUSEPORT` group the DNS sockets join: two processes answering `/readyz` for one node would look like two instances.
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
endpoints = ["mcp", "healthz", "readyz"]
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable the control plane listener |
| `listen_addr` | `"127.0.0.1"` | Bind address; IPv4 or IPv6 literal (`[::1]` also accepted) |
| `port` | `19090` | Bind port |
| `token` | `None` | Bearer token for `/mcp` |
| `allowed_hosts` | `[]` | Allowed `Host` header values for DNS rebinding protection (`/mcp` only) |
| `endpoints` | `["mcp", "healthz", "readyz"]` | Which routes the listener serves |

## Endpoints

| Route | Method | Auth | Purpose |
|-------|--------|------|---------|
| `/mcp` | POST (JSON-RPC 2.0, optional SSE) | Bearer token when set | MCP tools |
| `/healthz` | GET | none | Liveness; `{"status":"ok"}`, 200 while the process is up |
| `/readyz` | GET | none | Live runtime signals report; always 200 — a report, not a verdict |

The MCP server listens on `http://<listen_addr>:<port>/mcp` using the
**Streamable HTTP** transport.

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

Returns live runtime signals sampled at call time — the same `signals` object
`/readyz` serves over GET — plus the current log filter directives under
`logs`. One signal per registered source:

**Input:** `{}` (no arguments)

**Output:**

```json
{
  "signals": {
    "nacos.nacos.local": {
      "subscribedServices": 42
    },
    "xdp": {
      "attached": true
    }
  },
  "logs": {
    "stdout": "info",
    "otlp": "info"
  }
}
```

`subscribedServices` counts the names doggy-dns has subscribed; instance data
lives in the Nacos client's cache, so the control plane reports no cache size
or push timestamp. `xdp.attached` is only reported by builds with the fast path
wired in; the flag is fixed after startup because dropping the manager detaches
the program.

### `set_log_level`

Replaces or restores the process's log filters (see
[observability.md](../../docs/observability.md#runtime-log-level)); the current
directives are part of `get_runtime_status`. It touches no configuration file,
nothing on disk, and nothing that affects DNS answers; the change does not
survive a restart.

**Input:** exactly one of `directives` and `reset`.

```json
{ "directives": "doggy_dns_nacos=debug,hickory_proto=info" }
```

- `directives` — `EnvFilter`-style string applied to every enabled log output,
  replacing its filter wholesale
- `"reset": true` — restore the filters each output started with
- passing both is rejected; so are passing neither, and unparsable directives,
  which leave the current filter untouched

**Output:** the directives now in force; `null` means that output is disabled.

```json
{ "stdout": "doggy_dns_nacos=debug,hickory_proto=info", "otlp": "doggy_dns_nacos=debug,hickory_proto=info" }
```

Every filter installed on the OTLP output appends exclusions for the OTel SDK's
own crates, so the bridge never re-exports the SDK's logs.

## Authentication

When `[control].token` is set, requests to `/mcp` must include:

```
Authorization: Bearer <token>
```

`/mcp` requests without a valid token receive `401 Unauthorized`.
`/healthz` and `/readyz` never require auth — kubelets must not hold secrets.

## Contract Stability

The following JSON field names are part of the stable contract and must not be renamed:

- `get_status` → `instanceId`, `host`, `version`, `pid`, `uptimeSeconds`, `startedAt`, `listenAddr`, `protocols`, `status`
- `list_plugins` → `plugins[].order`, `kind`, `enabled`, `details`
- `get_config` → `format`, `content`, `redactedKeys`
- `get_runtime_status` → `signals`, `logs.stdout`, `logs.otlp`
- `GET /readyz` → `signals`
- `set_log_level` → `stdout`, `otlp`
- `GET /healthz` → `status`

## Smoke Test

```bash
# Enable the control plane in config/doggy-dns.toml ([control] enabled = true), then:
cargo run -- config/doggy-dns.toml

# Liveness and readiness probes (no auth, always 200)
curl -s http://127.0.0.1:19090/healthz
curl -s http://127.0.0.1:19090/readyz

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

## Mutating Surface

`set_log_level` is the only tool that changes process state, and the only state
it changes is the in-memory log filter. It declares `readOnlyHint = false`,
`destructiveHint = false` and `idempotentHint = true`; the other four tools
declare `readOnlyHint = true`. There are no tools, resources, or prompts that
modify configuration, clear caches, restart services, or perform any
destructive action.
