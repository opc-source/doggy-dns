# XDP Kernel Fast Path

Doggy-DNS can answer repeated DNS queries **without leaving kernel space**:
an eBPF XDP program attached to a network interface serves cached replies
directly, and only cache misses fall through to the userspace server. Built
with [aya](https://github.com/aya-rs/aya).

Part of [README](../README.md); see also
[configuration.md](configuration.md) and
[architecture.md](architecture.md).

## Requirements

- Linux (the fast path is a no-op stub on other platforms).
- `bpf-linker` on `PATH` and a nightly toolchain for the `xdp/` workspace
  (pinned by `xdp/rust-toolchain.toml`); the eBPF program is built
  automatically by `crates/doggy-dns-xdp/build.rs` when the binary is built
  with `--features xdp` — a zero-byte placeholder is embedded if the build
  environment cannot compile it, and loading then fails at startup with a
  warning.
- Root (or `CAP_BPF` + `CAP_NET_ADMIN`) to load and attach the program, and a
  bpffs mounted (usually `/sys/fs/bpf`) for the pinned cache map.

## Build & run

```bash
# 1. Build the binary with the fast path (the eBPF program is compiled by build.rs)
cargo build --release --features xdp

# 2. Enable it in the config (see the [xdp] block in docs/configuration.md)
#    [xdp]
#    enabled = true

# 3. Run as root; the interface comes from config / XDP_INTERFACE / auto-detect
sudo RUST_LOG=info ./target/release/doggy-dns config/doggy-dns.toml

# Verify attachment
ip -d link show <iface>        # xdp program appears under "prog"
```

The first query for a name goes through the normal middleware → authority
pipeline; the response is written into the BPF map on the way out. Identical
queries within the TTL are then answered by the kernel (`XDP_TX`).

## `[xdp]` configuration

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `false` | Master switch; a failed init degrades to running without the fast path. |
| `interface` | `""` | Resolve order: config value → `XDP_INTERFACE` env → default route parsed from `/proc/net/route`. |
| `cache_size` | `65536` | **Currently informational** — the map size is fixed by the eBPF program (`DNS_CACHE` is declared with 65536 entries) and the loader does not override it. |
| `negative_ttl` | `30` | Seconds a cached NXDOMAIN is served. Also the fallback TTL when a positive answer exposes none. |
| `attach_mode` | `"auto"` | `auto`: attach to the interface at startup. `manual`: load only, leaving attachment to an external tool. |
| `pin_path` | `/sys/fs/bpf/doggy-dns/dns_cache` | Where the cache map is pinned so it survives (and can be re-attached across) restarts. |

## How it works

### Runtime architecture

```
        +======================================================================+
        | INGRESS on <iface>   (XDP_DEFAULT hook; UDP traffic only)            |
        |                                                                      |
        |   1  eth / IPv4 / UDP?                     no -> XDP_PASS            |
        |   2  dport == XDP_CONFIG.dns_port?         no -> XDP_PASS            |
        |   3  QR=0 and QDCOUNT=1?                   no -> XDP_PASS            |
        |   4  FNV-1a key over qname + qtype + qclass                          |
        |   5  DNS_CACHE.get(key)      miss/expired -> XDP_PASS (counted)      |
        |   6  bpf_xdp_adjust_tail, then:                                      |
        |         positive hit : copy cached reply (<= 512 B), patch TXID      |
        |         negative hit : synthesise the NXDOMAIN header                |
        |         pre-resize failure  -> XDP_PASS                              |
        |         post-resize failure -> XDP_DROP (query already rewritten)    |
        |   7  swap MAC/IP/ports, fix lengths, RFC1624 incremental checksum    |
        |   8  XDP_TX  (no context switch; userspace never sees the query)     |
        +======================================================================+
             BPF maps     DNS_CACHE  LruHashMap, 65536 entries
                         XDP_CONFIG Array[1] {dns_port}, written pre-attach
                         XDP_STATS  PerCpuArray {hit, negative_hit, miss,
                                                  expired, drop}
               kernel reads ^   userspace writes (map pinned at pin_path)
                             |
      miss -> stack -> socket|       ^ hit -> XDP_TX straight to the client
                             v       |
    +======================================================================+
    | USERSPACE doggy-dns (cargo feature "xdp")                            |
    |                                                                      |
    |   UDP :port -> middleware chain -> AuthorityChain                    |
    |                                   (nacos / native / forward)         |
    |   XdpCacheMiddleware.after_request -> DNS_CACHE.insert               |
    |       NoError entry : min answer TTL                                 |
    |       NXDomain entry: negative_ttl                                   |
    |   observability: XDP_STATS poller -> OTLP dns.xdp.queries{result=..} |
    +======================================================================+
```

Mermaid version of the same decision path:

```mermaid
flowchart TD
    A[Packet arrives on iface] --> B{IPv4 / UDP / port == dns_port?}
    B -- no --> P[XDP_PASS to network stack]
    B -- yes --> C{QR=0 and QDCOUNT=1?}
    C -- no --> P
    C -- yes --> D[Hash QNAME + qtype/qclass]
    D --> E{DNS_CACHE entry?}
    E -- miss / expired --> P
    E -- fresh --> F{negative?}
    F -- yes --> G[Adjust tail, synthesise NXDOMAIN header]
    F -- no --> H[Adjust tail, copy <=512B reply, patch TXID]
    G --> I[Swap headers, fix lengths, checksum]
    H --> I
    I --> J[XDP_TX reply]
    P -.userspace resolves.-> K[XdpCacheMiddleware writes entry]
    K -.next identical query.-> E
```

### First query vs cached query

```mermaid
sequenceDiagram
    participant C as Client
    participant K as dns_xdp (kernel)
    participant U as doggy-dns (userspace)
    participant A as Authority chain
    C->>K: query (miss)
    K->>U: XDP_PASS
    U->>A: middleware -> authority chain
    A-->>U: response + min answer TTL
    U-->>C: reply
    U->>K: insert CacheKey/CacheValue into DNS_CACHE
    C->>K: identical query within TTL
    K-->>C: XDP_TX cached reply (userspace never sees it)
```

### Kernel side (`xdp/doggy-dns-xdp-ebpf`)

For every inbound packet the program runs a strict fast path; anything it
does not recognise is `XDP_PASS`ed to the normal network stack untouched:

1. **Parse headers** — Ethernet (IPv4 only), IPv4, UDP. Non-DNS traffic and
   any packet addressed to a port other than the server's `dns_port` (read
   from the `XDP_CONFIG` map the loader fills in before attach) pass through.
2. **Validate the DNS query** — DNS header present, `QR = 0`, `QDCOUNT = 1`;
   otherwise pass.
3. **Compute the cache key** — FNV-1a over the QNAME's raw wire bytes
   (length prefixes included, lowercased) plus `qtype`/`qclass`. The
   userspace key builder produces the identical hash.
4. **Look up `DNS_CACHE`** (an LRU hash map) — a miss or an entry whose
   `expires_at` (`bpf_ktime_get_ns` clock) has passed is passed to userspace.
5. **Rebuild the reply in place** — `bpf_xdp_adjust_tail` resizes the frame
   to the response length; positive hits copy the cached DNS message
   (chunked, bounds-checked, capped at 512 bytes) and overwrite the transaction
   ID with the current query's; negative hits synthesise an NXDOMAIN header.
   MACs/IPs/ports are swapped, IP total length and UDP length fixed, and the
   IPv4 checksum updated incrementally (RFC 1624). The packet goes back with
   `XDP_TX`.

After a resize the original query no longer exists, so any later failure
drops the packet rather than passing a truncated frame upward.

### Userspace side (`crates/doggy-dns-xdp` + `src/xdp/`)

- **`XdpManager`** loads the embedded ELF, takes and pins `DNS_CACHE`, wraps
  `XDP_STATS`, writes `XDP_CONFIG { dns_port }`, then loads the program and
  (in `auto` mode) attaches it to the interface. The manager must stay alive
  for the server lifetime — dropping it detaches the program.
- **`XdpCacheMiddleware`** is appended to the middleware chain on success. In
  `after_request` it caches only `NoError` and `NXDomain` responses that carry
  the serialized reply: positive entries expire at the minimum TTL across the
  answer records, negative entries at `negative_ttl`. Expiry uses
  `CLOCK_MONOTONIC` so userspace timestamps share the kernel's `bpf_ktime_get_ns`
  clock. A cache write failure is logged and the request still succeeds.
- **Stats**: the program counts `hit` / `negative_hit` / `miss` / `expired` /
  `drop` in a per-CPU array. A poller snapshots the map every
  `observability.metrics.interval_secs`, converts snapshots to deltas
  (reload-safe: counters that went backwards are treated as a fresh map), and
  exports them as the OTLP metric `dns.xdp.queries`, labelled by result. If
  OTLP metrics are disabled the counters are collected but not exported (a
  warning says so at startup).

## Limitations & notes

- **UDP only** — XDP sees inbound UDP queries; TCP/DoT/DoH always go through
  userspace.
- **One XDP program per interface** in default mode — a second attach fails
  with `EBUSY`.
- **Cache admission is limited to 512-byte replies** — the response is
  serialized with the client's EDNS payload size, then cached only if the
  actual wire length is ≤512 bytes and the reply is not truncated. EDNS
  queries with small answers are cached; larger or truncated answers are
  served from userspace and never cached.
- **Keyed by 64-bit FNV-1a hash, not the name itself** — a collision would
  serve another name's answer. Acceptable for a private resolver cache, worth
  knowing before exposing it publicly.
- `cache_size` does not currently resize the map (see the table above).
- The kernel program has no logging today; silent passes are observable only
  through the `miss` / `expired` / `drop` counters.

## Development

```bash
cargo check --workspace --features xdp          # userspace crate compiles on any OS
cargo test -p doggy-dns-xdp                     # pure logic: key hashing, deltas, config
cargo build -p doggy-dns --features xdp         # full binary incl. eBPF build.rs step
cd xdp && cargo build -Z build-std=core,alloc --release --target bpfel-unknown-none  # eBPF only
sudo -E cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored  # load + attach on loopback (Linux, root)
cargo bench -p doggy-dns-xdp                    # component benchmarks
```

The BPF struct layout (`CacheKey`, `CacheValue`, `XdpStats`,
`XdpRuntimeConfig`) is intentionally duplicated between
`crates/doggy-dns-xdp/src/types.rs` and
`xdp/doggy-dns-xdp-ebpf/src/types.rs`; `XdpStats` has a drift-guard test, but
the others must be edited in both places together.
