# Doggy-DNS

狗狗 DNS，可以 DaemonSet 或中心化权威部署？暂时不完善，但作为本地代理完全冇问题。想试试 AI Coding 有多强，一天做了在以前应该需要一两个月的事，主要是作者水平不够而它拔高了所有技术能力。几乎所有代码由 AI 生成，只小改了些字符！！！

A Rust-based service discovery DNS server built on [hickory-dns](https://github.com/hickory-dns/hickory-dns), inspired by CoreDNS's plugin architecture. Resolves service names from registries like Nacos to IP addresses via standard DNS queries.

## Architecture

```
                          DNS Query (UDP/TCP/TLS/HTTPS)
                                      |
                                      v
                           +--------------------+
                           |  DoggyDnsHandler   |
                           +--------------------+
                                      |
                    +-----------------+-----------------+
                    |                                   |
                    v                                   v
             Middleware Chain                     Authority Chain
             (before / after)                   (sequential lookup)
                    |                                   |
          +---------+---------+           +-------------+-------------+
          |                   |           |             |             |
          v                   v           v             v             v
      Logging            Metrics        Nacos         Native       Forward
    Middleware          Middleware    Authority     Authority      Authority
```

Queries pass the middleware chain, then the authority chain (first non-`Skip`
handler wins; all-skip answers `NXDOMAIN`), then the middleware chain again on
the way out. Details — dependency graph, plugin axes, Nacos integration,
protocol ports, log events — in [docs/architecture.md](docs/architecture.md).

```
doggy-dns/                    # Binary entrypoint
  crates/
    doggy-dns-basic/          # Config types, loading & validation (serde/toml/anyhow only)
    doggy-dns-plugin/         # Plugin traits, authority chain, built-in authorities
    doggy-dns-core/           # Server runtime, handler, middleware (config re-exported from basic)
    doggy-dns-nacos/          # Nacos service discovery backend
    doggy-dns-observability/  # OTLP logs/metrics/traces/profiles export
    doggy-dns-mcp/            # Read-only MCP server for console introspection
    doggy-dns-xdp/            # Optional XDP kernel fast path (feature = "xdp", Linux only)
  xdp/                        # eBPF program (separate workspace, target bpfel-unknown-none)
```

## Features

- **Service discovery**: Nacos (`<service>.<group>.<namespace>.<zone>.`), OS
  system resolver, and explicit upstream forwarding, chained by config order.
- **Protocols**: UDP/TCP on the configured port; DoT (853) and DoH (443,
  `/dns-query`) when `[server.tls]` / `[server.https]` are enabled.
- **OTLP observability**: logs/metrics/traces on by default, CPU profiles opt-in;
  presets for OpenObserve, OTel Collector, Vector. → [docs/observability.md](docs/observability.md)
- **MCP server**: read-only introspection (`get_status`, `list_plugins`,
  `get_config` with secrets redacted) over Streamable HTTP. → [crates/doggy-dns-mcp/README.md](crates/doggy-dns-mcp/README.md)
- **XDP fast path** (optional, Linux): answers cached DNS queries in the kernel
  via an aya eBPF program. → [docs/xdp.md](docs/xdp.md)

## Configuration

TOML-based; default path `config/doggy-dns.toml` (or pass as CLI arg). Minimal
working config:

```toml
[server]
listen_addr = "0.0.0.0"
port = 15353
worker_threads = 1

[middleware]
logging = true

# Plugins are evaluated in order; first non-Skip result wins.
# If no [[plugins]] are configured, defaults to forwarding to 8.8.8.8 / 1.1.1.1.
[[plugins]]
kind = "native"
enabled = true
```

Full reference (TLS/HTTPS, observability, Nacos, remote config, MCP):
[docs/configuration.md](docs/configuration.md).

## Quick Start

```bash
cargo build --release
./target/release/doggy-dns config/doggy-dns.toml

# Debug logging
RUST_LOG=doggy_dns=debug ./target/release/doggy-dns config/doggy-dns.toml

# Query it
dig @127.0.0.1 -p 15353 www.google.com A
dig @127.0.0.1 -p 15353 user-service.DEFAULT_GROUP.public.nacos.local A

# Tests / benchmarks / coverage
cargo test --workspace
cargo bench --workspace
cargo tarpaulin --config tarpaulin.toml
```

## Documentation

| Doc | Contents |
|-----|----------|
| [docs/architecture.md](docs/architecture.md) | Dependency graph, request flow, plugins, Nacos, protocols, log events |
| [docs/configuration.md](docs/configuration.md) | Full TOML reference, validation rules, MCP |
| [docs/observability.md](docs/observability.md) | OTLP signals, backend presets, metrics/traces/profiles, troubleshooting |
| [docs/xdp.md](docs/xdp.md) | XDP kernel fast path: build, enable, how it works, limits |
| [docs/development.md](docs/development.md) | Build/test/bench commands, coding rules, CI, test & benchmark structure, dependencies |
| [AGENTS.md](AGENTS.md) | The above conventions mirrored for AI coding agents |

## Roadmap

- [ ] Kubernetes service discovery (`kube-rs` / `k8s-openapi`)
- [ ] etcd backend (`etcd-rs`)
- [ ] Redis backend (`redis-rs`)
- [ ] Database backend (`sqlx`)
- [ ] Dynamic plugin registry (load plugins at runtime)
- [ ] Config hot-reload via Nacos config service (watcher implemented, wiring pending)
- [x] OTLP/gRPC export (HTTP/protobuf is supported today)
- [x] Multi-target OTLP fan-out (use a Collector/Vector today)

## License

[Apache License Version 2.0](LICENSE)

## Acknowledgement

- Inspired by DNS-F from Alibaba and [CoreDNS](https://coredns.io)
- Built on [hickory-dns](https://github.com/hickory-dns/hickory-dns)
