# dns-filter
A Rust based DNS Filter, based on `hickory-dns`.

DnsFilter 主要由 `hickory-dns` 承载，同等一部分 CoreDNS 的能力，借鉴插件式方案。
1. TODO 内置 nacos-sdk-rust -> nacos-server
2. TODO 内置 kube-rs/k8s-openapi -> k8s-apiserver
3. TODO etcd-rs -> etcd
4. TODO redis-rs -> redis
5. TODO sqlx -> Database


## Quick Start

### Build

```bash
cargo build --release
```

### Run

```bash
# With default config
./target/release/dns-filter config/dns-filter.toml

# With debug logging
RUST_LOG=dns_filter=debug ./target/release/dns-filter config/dns-filter.toml
```

### Test

```bash
# Run all tests
cargo test --workspace

# Query the server (once running)
dig @127.0.0.1 -p 53 user-service.DEFAULT_GROUP.public.nacos.local A
```

# License
[Apache License Version 2.0](LICENSE)

# Acknowledgement
- Inspired by DNS-F from Alibaba, CoreDNS
- A Rust based DNS server you known Trust-DNS [hickory-dns](https://github.com/hickory-dns/hickory-dns.git)
