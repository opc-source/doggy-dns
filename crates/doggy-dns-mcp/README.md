# doggy-dns-mcp

Read-only MCP (Model Context Protocol) server for doggy-dns instance introspection.

## Overview

This crate provides a lightweight, read-only MCP server that exposes the running doggy-dns instance's status, plugin configuration, and redacted configuration file. It is designed for integration with the doggy-dns console and other AI-powered management tools.

**Key properties:**

- **Disabled by default** — set `[mcp].enabled = true` to activate.
- **Read-only** — all three tools carry `readOnlyHint = true`, `destructiveHint = false`.
- **Non-fatal** — if the MCP port is already in use, the DNS server continues running.
- **Config redaction** — `get_config` recursively replaces sensitive values (`password`, `token`, `secret`, `authorization`, `api_key`, `apikey`, `passwd`) with `"***"`.
- **Optional Bearer auth** — set `[mcp].token` to require `Authorization: Bearer <token>` on all requests.

## Configuration

```toml
[mcp]
enabled = true
listen_addr = "127.0.0.1"
port = 19090
token = "change-me"                    # omit for no auth
allowed_hosts = ["dns-node-1", "10.0.0.5:19090"]
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable the MCP server |
| `listen_addr` | `"127.0.0.1"` | Bind address |
| `port` | `19090` | Bind port |
| `token` | `None` | Bearer token for authentication |
| `allowed_hosts` | `[]` | Allowed `Host` header values for DNS rebinding protection |

## Endpoint

The MCP server listens on `http://<listen_addr>:<port>/mcp` using the **Streamable HTTP** transport (JSON-RPC 2.0 over HTTP POST with optional SSE streaming).

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

**Input:** `{}` (no arguments)

**Output:**

```json
{
  "format": "toml",
  "content": "[server]\nlisten_addr = \"0.0.0.0\"\nport = 15353\n...",
  "redactedKeys": ["observability.basic_auth.password"]
}
```

If the config file cannot be read:

```json
{
  "error": {
    "code": "config_unreadable",
    "message": "failed to read config file: ..."
  }
}
```

## Authentication

When `[mcp].token` is set, all requests must include:

```
Authorization: Bearer <token>
```

Requests without a valid token receive `401 Unauthorized`.

## Contract Stability

The following JSON field names are part of the stable contract and must not be renamed:

- `get_status` → `instanceId`, `host`, `version`, `pid`, `uptimeSeconds`, `startedAt`, `listenAddr`, `protocols`, `status`
- `list_plugins` → `plugins[].order`, `kind`, `enabled`, `details`
- `get_config` → `format`, `content`, `redactedKeys`

## Smoke Test

```bash
# Start doggy-dns with MCP enabled
cargo run -- config/doggy-dns.toml

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
