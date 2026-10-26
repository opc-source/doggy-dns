# XDP Fast Path Design

## Overview

Add an eBPF/XDP cache layer to doggy-dns that intercepts DNS queries at the network interface level. Cache hits are served directly in kernel space via XDP_TX, bypassing the entire userspace pipeline. Cache misses pass through to the existing hickory-server pipeline unchanged. A new middleware (`XdpCacheMiddleware`) writes resolved responses back into the BPF map for future fast-path hits.

Inspired by Knot DNS's XDP answer-from-cache mode, adapted for doggy-dns's middleware/authority-chain architecture.

## Goals

- Reduce DNS query latency for cached responses to microsecond-level (bypass kernel network stack + userspace processing)
- Support both positive (NoError) and negative (NXDomain) caching
- Zero impact on existing functionality when XDP is disabled or fails to attach
- Deployable as a K8s DaemonSet with automatic network interface detection
- Clean integration with existing middleware chain and BPF map lifecycle

## Non-Goals (MVP)

- XDP-level observability metrics (hit/miss counters exposed via Prometheus) — deferred to follow-up
- TCP / DoT / DoH acceleration — XDP only handles UDP :53
- Multi-queue / per-CPU XDP workers with SOCKMAP — single-queue XDP is sufficient for MVP
- Custom DNS resolution logic in XDP (e.g., CNAME chaining, EDNS client subnet)

## Architecture

```
 K8s Node / Linux Host
 ┌──────────────────────────────────────────────────────────────┐
 │                                                              │
 │  Physical NIC (eth0 / auto-detected default route iface)     │
 │   │                                                          │
 │   ▼ UDP :53                                                  │
 │   ┌────────────────────────────────────────┐                 │
 │   │  XDP Program (eBPF, kernel space)      │                 │
 │   │                                        │                 │
 │   │  Parse UDP → DNS question              │                 │
 │   │  → Lookup BPF LRU hash map             │                 │
 │   │  → HIT:  build response, XDP_TX        │                 │
 │   │  → MISS: XDP_PASS (fall through)       │                 │
 │   └────────────────────┬───────────────────┘                 │
 │                        │ XDP_PASS                            │
 │                        ▼                                      │
 │   ┌────────────────────────────────────────┐                 │
 │   │  Existing Userspace Pipeline           │                 │
 │   │  (unchanged)                            │                 │
 │   │                                        │                 │
 │   │  hickory-server → Middleware chain      │                 │
 │   │   ├─ LoggingMiddleware                 │                 │
 │   │   ├─ DnsMetricsMiddleware              │                 │
 │   │   └─ XdpCacheMiddleware (NEW)          │                 │
 │   │       after_request → write BPF map    │                 │
 │   │                                        │                 │
 │   │  AuthorityChain                        │                 │
 │   │   ├─ NacosAuthority                    │                 │
 │   │   ├─ NativeAuthority                   │                 │
 │   │   └─ ForwardAuthority                  │                 │
 │   └────────────────────────────────────────┘                 │
 │                                                              │
 │  /sys/fs/bpf/doggy-dns/dns_cache  (pinned BPF map)           │
 └──────────────────────────────────────────────────────────────┘
```

### Data Flow

**First query (cache miss):**
1. Packet arrives at NIC → XDP program parses UDP + DNS
2. BPF map lookup misses (or entry expired) → `XDP_PASS`
3. Packet enters kernel network stack → received by hickory-server UDP socket
4. Existing middleware chain processes the query (`before_request`)
5. AuthorityChain resolves through configured plugins
6. Response is sent back to client
7. `XdpCacheMiddleware::after_request()` writes the response into the BPF map

**Subsequent identical query (cache hit):**
1. Packet arrives at NIC → XDP program parses UDP + DNS
2. BPF map lookup hits, TTL valid
3. XDP program constructs response in-place (swap headers, copy/modify payload)
4. `XDP_TX` bounces the packet back on the same NIC → response sent to client
5. The userspace pipeline never sees this packet

**NXDomain query (negative cache hit):**
1. Same as cache hit, but XDP program sets `RCODE=3`, `ANCOUNT=0`, truncates after question section
2. Much simpler than positive hit — no answer data to copy

## BPF Map Design

### Map Type

`BPF_MAP_TYPE_LRU_HASH` — automatic LRU eviction when the map is full. No manual GC or background cleanup thread needed. Entries are also checked for TTL expiry on lookup.

### Key Structure

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct CacheKey {
    name_hash: u64,     // FNV-1a 64-bit hash of lowercased QNAME
    qtype_class: u32,   // (qtype << 16) | qclass
}
// sizeof = 12 bytes, padded to 16 by BPF allocator
```

**Why hash the QNAME:** BPF verifier restricts loops and variable-length operations. A fixed-size 64-bit hash avoids string comparison in the XDP program. FNV-1a collision probability is ~2^-64 per pair, negligible for DNS cache workloads.

**Why pack qtype+qclass:** Keeps the key compact (16 bytes total with padding) while covering all record types and IN/CH classes.

### Value Structure

```rust
#[repr(C)]
#[derive(Clone, Copy)]
struct CacheValue {
    resp_data: [u8; 512],   // Full DNS response bytes (header + question + answers)
    resp_len: u16,           // Actual length of resp_data
    flags: u16,              // 0x00 = positive, 0x01 = negative (NXDOMAIN)
    _pad: u32,
    expires_at: u64,         // Absolute kernel timestamp (bpf_ktime_get_ns)
}
// sizeof = 528 bytes
```

**Why store the full response:** The XDP program does not need to understand DNS answer record format. On a positive cache hit, it simply copies `resp_data` into the packet buffer. This keeps the BPF program minimal and verifiable.

**Memory cost:** 528 bytes × 65536 entries = ~33 MB per NUMA node. Acceptable for a DNS server.

### Map Sizing

- Default `max_entries`: 65536
- Configurable via `config.xdp.cache_size`
- LRU semantics mean the map self-regulates — no overflow errors, oldest unused entries are evicted automatically

### TTL Strategy

**Positive cache:** `expires_at = write_time + min(TTL of all answer RRs)`. The middleware extracts the minimum TTL from the DNS response's answer section at write time.

**Negative cache (NXDomain):** `expires_at = write_time + config.xdp.negative_ttl`. A single fixed TTL for all negative entries, configurable (default 30 seconds).

**Expired entries:** The XDP program checks `expires_at > bpf_ktime_get_ns()` on every lookup. Expired entries are treated as misses. The LRU map will eventually reclaim the slot.

## XDP Program Logic

### Entry Point

```
#[xdp] fn dns_xdp(ctx: XdpContext) -> u32
```

### Processing Steps

1. **Protocol filter:**
   - Parse Ethernet header → check `ether_type == IPv4` (IPv6 deferred)
   - Parse IPv4 header → check `protocol == UDP`
   - Parse UDP header → check `dest_port == 53`
   - Any check fails → `XDP_PASS`

2. **DNS parsing:**
   - Locate DNS payload after UDP header
   - Check `QR == 0` (must be a query, not a response)
   - Check `QDCOUNT == 1` (single question; multi-question queries → `XDP_PASS`)
   - Extract QNAME (uncompress label-by-label), QTYPE, QCLASS
   - Parse failure → `XDP_PASS`

3. **Cache lookup:**
   - Compute `FNV-1a(QNAME.lowercase())` → `name_hash`
   - Pack `(QTYPE << 16) | QCLASS` → `qtype_class`
   - `bpf_map_lookup_elem(&DNS_CACHE, &key)`
   - Not found → `XDP_PASS`

4. **TTL check:**
   - `now = bpf_ktime_get_ns()`
   - `entry.expires_at <= now` → `XDP_PASS` (expired, let LRU reclaim later)

5. **Response construction:**

   **Positive hit (flags == 0):**
   - Call `bpf_xdp_adjust_tail()` to set packet length = `ETH_HDR + IP_HDR + UDP_HDR + entry.resp_len`
   - Copy `entry.resp_data` into the DNS payload region (offset past L3+L4 headers)
   - Overwrite the Transaction ID in the response DNS header with the original query's Transaction ID

   **Negative hit (flags & 0x01):**
   - Call `bpf_xdp_adjust_tail()` to truncate packet to `ETH_HDR + IP_HDR + UDP_HDR + question_len`
   - Modify DNS header in-place: set `QR=1`, `RCODE=3` (NXDomain), `ANCOUNT=0`, `NSCOUNT=0`, `ARCOUNT=0`

6. **Header swap:**
   - Swap source and destination MAC addresses
   - Swap source and destination IPv4 addresses
   - Swap source and destination UDP ports
   - Recalculate IPv4 header checksum
   - Recalculate UDP checksum (covers IP pseudo-header + UDP payload)
   - Set `IP_DF` (don't fragment) flag

7. **Return `XDP_TX`** — packet is transmitted back on the same NIC

### BPF Helper Constraints

- No `bpf_trace_printk` in production (debug only)
- No unbounded loops (QNAME label parsing uses bounded iteration, max 128 labels)
- All memory accesses must be within packet bounds (verified by BPF verifier)
- `bpf_xdp_adjust_tail()` can only shrink or grow within the XDP frame bounds (typically 1500 bytes for standard MTU, up to 65535 for jumbo frames)

## Userspace Components

### Crate: `doggy-dns-xdp`

```
crates/doggy-dns-xdp/
├── Cargo.toml
└── src/
    ├── lib.rs           # XdpManager: public API, lifecycle
    ├── program.rs       # eBPF ELF loading, XDP attach/detach
    ├── cache.rs         # BPF map read/write wrappers
    ├── middleware.rs     # XdpCacheMiddleware (implements Middleware trait)
    └── interface.rs     # Network interface auto-detection
```

### XdpManager

```rust
pub struct XdpManager {
    _link: Option<XdpLink>,         // Holds the XDP attachment; drop = detach
    cache_map: HashMap<CacheKey, CacheValue>,  // Aya handle on the BPF map
    interface: String,
}

impl XdpManager {
    /// Load eBPF program, attach to interface, open BPF map.
    /// Returns Err on any failure — caller should log warning and continue without XDP.
    pub fn new(config: &XdpConfig) -> Result<Self>;

    /// Write a cache entry. Called by XdpCacheMiddleware.
    pub fn cache_response(&self, key: &CacheKey, value: &CacheValue) -> Result<()>;

    /// Detach XDP program and clean up. Called on shutdown.
    pub fn detach(self);
}
```

### XdpCacheMiddleware

```rust
pub struct XdpCacheMiddleware {
    cache: Arc<HashMap<CacheKey, CacheValue>>,  // Shared BPF map handle
    negative_ttl_secs: u64,
}

#[async_trait]
impl Middleware for XdpCacheMiddleware {
    async fn before_request(&self, _req: &Request) -> MiddlewareAction {
        // No-op: XDP cache hits never reach userspace.
        // This middleware only populates the cache on the response path.
        MiddlewareAction::Continue
    }

    async fn after_request(&self, req: &Request, _duration_ms: u64, outcome: &RequestOutcome) {
        // Only cache successful responses (NoError) and NXDomain
        if outcome.response_code != ResponseCode::NoError
            && outcome.response_code != ResponseCode::NXDomain
        {
            return;
        }

        // Only cache if we have answer data or it's an NXDomain
        let question = req.question();
        let key = CacheKey {
            name_hash: fnv1a_64(&question.name().to_string().to_lowercase()),
            qtype_class: ((question.query_type() as u32) << 16)
                       | (question.query_class() as u32),
        };

        let ttl = if outcome.response_code == ResponseCode::NXDomain {
            self.negative_ttl_secs
        } else {
            outcome.min_answer_ttl().unwrap_or(self.negative_ttl_secs)
        };

        let flags = if outcome.response_code == ResponseCode::NXDomain {
            FLAG_NEGATIVE
        } else {
            0
        };

        let value = CacheValue {
            resp_data: outcome.response_bytes(),
            resp_len: outcome.response_len() as u16,
            flags,
            _pad: 0,
            expires_at: now_ktime_ns() + ttl * 1_000_000_000,
        };

        // Best-effort write; log on failure but don't affect response
        if let Err(e) = self.cache.insert(&key, &value, UpdateFlags::ANY) {
            tracing::warn!(error = %e, "failed to write XDP cache entry");
        }
    }
}
```

### Interface Auto-Detection

```rust
/// Detect the default network interface by parsing /proc/net/route.
/// Returns the interface name (e.g., "eth0") for the default route entry.
pub fn detect_default_interface() -> Result<String> {
    let content = std::fs::read_to_string("/proc/net/route")
        .context("failed to read /proc/net/route")?;

    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Format: Iface Destination Gateway Flags RefCnt Use Metric Mask MTU ...
        // Default route has Destination = 00000000
        if fields.len() >= 2 && fields[1] == "00000000" {
            return Ok(fields[0].to_string());
        }
    }

    Err(anyhow::anyhow!("no default route found in /proc/net/route"))
}

/// Resolve the XDP target interface with priority:
/// 1. config.xdp.interface (explicit)
/// 2. XDP_INTERFACE environment variable
/// 3. Auto-detect from /proc/net/route
pub fn resolve_interface(config: &XdpConfig) -> Result<String> {
    if !config.interface.is_empty() {
        return Ok(config.interface.clone());
    }
    if let Ok(iface) = std::env::var("XDP_INTERFACE") {
        if !iface.is_empty() {
            return Ok(iface);
        }
    }
    detect_default_interface()
}
```

### Graceful Degradation

```rust
// In main.rs or async_main:
let xdp_manager = if config.xdp.enabled {
    match XdpManager::new(&config.xdp) {
        Ok(mgr) => {
            tracing::info!(interface = %mgr.interface(), "XDP fast path active");
            Some(mgr)
        }
        Err(e) => {
            tracing::warn!(error = %e, "XDP attach failed, running in userspace-only mode");
            None
        }
    }
} else {
    None
};

// Middleware chain construction:
if let Some(ref mgr) = xdp_manager {
    middlewares.push(Arc::new(XdpCacheMiddleware::new(
        mgr.cache_handle(),
        config.xdp.negative_ttl,
    )));
}
```

**Key principle:** XDP failure never prevents DNS service from starting. The system falls back to pure userspace mode transparently.

## BPF Map Pinning

### Pin Path

Default: `/sys/fs/bpf/doggy-dns/dns_cache`

The bpffs path must be writable by the doggy-dns process. In K8s, this requires either:
- `securityContext.privileged: true`, or
- A writable bpffs mount at `/sys/fs/bpf` with appropriate permissions

### Lifecycle

| Event | Behavior |
|-------|----------|
| Pod starts, no pinned map | Create new BPF map, pin to path |
| Pod starts, pinned map exists | Open existing map (preserves cache across restarts) |
| Pod running | Map is shared between XDP program (kernel) and middleware (userspace) |
| Graceful shutdown | Unpin the map (optional — kernel reclaims when last fd closes) |
| Crash / OOM kill | Map persists in bpffs until all fds close; next pod startup reuses it |
| Pod deleted | Kernel automatically frees the map when all references are gone |

### Pin Path Configuration

For DaemonSet deployments with multiple pods on the same node (rare but possible), use a unique pin path per pod:

```toml
[xdp]
pin_path = "/sys/fs/bpf/doggy-dns/dns_cache"
```

Or via environment variable: `XDP_PIN_PATH=/sys/fs/bpf/doggy-dns-$POD_NAME/dns_cache`

## Configuration

### New Config Section

```toml
[xdp]
enabled = false
interface = ""
cache_size = 65536
negative_ttl = 30
attach_mode = "auto"
pin_path = "/sys/fs/bpf/doggy-dns/dns_cache"
```

### Field Descriptions

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Enable XDP fast path |
| `interface` | string | `""` | Target NIC name. Empty = auto-detect (see below) |
| `cache_size` | u32 | `65536` | Max entries in BPF LRU hash map |
| `negative_ttl` | u64 | `30` | TTL in seconds for NXDomain cache entries |
| `attach_mode` | string | `"auto"` | `"auto"` = program attaches XDP on startup; `"manual"` = external tool (bpftool/ip link) attaches, program only opens pinned map |
| `pin_path` | string | `"/sys/fs/bpf/doggy-dns/dns_cache"` | BPF map pin path for persistence |

### Interface Resolution Priority

1. `config.xdp.interface` (non-empty)
2. Environment variable `XDP_INTERFACE` (non-empty)
3. Auto-detect default route interface from `/proc/net/route`

### Config Validation

Add to `validate_config()`:

```rust
if config.xdp.enabled {
    if config.xdp.cache_size == 0 {
        return Err("xdp.cache_size must be > 0");
    }
    if config.xdp.negative_ttl == 0 {
        return Err("xdp.negative_ttl must be > 0");
    }
    if !["auto", "manual"].contains(&config.xdp.attach_mode.as_str()) {
        return Err("xdp.attach_mode must be 'auto' or 'manual'");
    }
    if !config.xdp.pin_path.starts_with("/sys/fs/bpf/") {
        return Err("xdp.pin_path must be under /sys/fs/bpf/");
    }
}
```

## K8s DaemonSet Deployment

### Required Pod Spec

```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: doggy-dns
spec:
  template:
    spec:
      hostNetwork: true              # Required: access host NIC for XDP attach
      dnsPolicy: ClusterFirstWithHostNet
      containers:
      - name: doggy-dns
        image: doggy-dns:latest
        securityContext:
          capabilities:
            add:
            - NET_ADMIN              # Required: XDP attach to NIC
            - SYS_RESOURCE           # Required: memlock for BPF maps
            # CAP_BPF (kernel 5.8+) may also be needed depending on kernel config
        env:
        - name: XDP_INTERFACE        # Override per-node (optional)
          value: ""                  # Empty = auto-detect
        volumeMounts:
        - name: bpffs
          mountPath: /sys/fs/bpf     # Required: BPF map pinning
      volumes:
      - name: bpffs
        hostPath:
          path: /sys/fs/bpf
          type: DirectoryOrCreate
```

### Node Kernel Requirements

- Linux kernel >= 5.10 (for stable XDP + BTF/CO-RE support)
- Kernel config: `CONFIG_BPF=y`, `CONFIG_BPF_SYSCALL=y`, `CONFIG_XDP_SOCKETS=y` (optional), `CONFIG_DEBUG_INFO_BTF=y` (for CO-RE)

### Fallback on Unsupported Nodes

If XDP attach fails (old kernel, missing capabilities, no bpffs mount), doggy-dns logs a warning and runs in pure userspace mode. DNS service is never interrupted.

## Build Integration

### Workspace Structure

```
doggy-dns/                          (main workspace)
├── Cargo.toml
│   [features]
│   xdp = ["doggy-dns-xdp"]
│   default = []
│
│   [workspace]
│   members = [
│       ".",
│       "crates/doggy-dns-core",
│       "crates/doggy-dns-plugin",
│       "crates/doggy-dns-nacos",
│       "crates/doggy-dns-observability",
│       "crates/doggy-dns-xdp",       # Always a member; only compiled with --features xdp
│   ]
│
├── crates/doggy-dns-xdp/            # Userspace crate (std, depends on aya)
│   ├── Cargo.toml
│   ├── build.rs                      # Compiles eBPF program if ELF is stale
│   └── src/
│
└── xdp/                             # Separate workspace (bpf target)
    ├── Cargo.toml
    └── doggy-dns-xdp-ebpf/
        ├── Cargo.toml                # depends on aya-ebpf, aya-log-ebpf
        └── src/main.rs
```

### Build Commands

```bash
# Build eBPF program (produces ELF at xdp/target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf)
cd xdp && cargo build --target bpfel-unknown-none --release

# Build doggy-dns with XDP support
cargo build --features xdp --release

# Build doggy-dns without XDP (existing behavior, no eBPF dependency)
cargo build --release
```

### build.rs Automation

`crates/doggy-dns-xdp/build.rs` checks if the eBPF ELF is up-to-date:
1. Compute hash of `xdp/doggy-dns-xdp-ebpf/src/**/*.rs` and `Cargo.toml`
2. Compare with stored hash in `OUT_DIR`
3. If stale, run `cargo build --target bpfel-unknown-none --release` in `xdp/`
4. Copy ELF to `OUT_DIR/doggy-dns-xdp-ebpf`
5. `lib.rs` uses `include_bytes!(concat!(env!("OUT_DIR"), "/doggy-dns-xdp-ebpf"))` to embed the program

This ensures the eBPF program is always rebuilt when its source changes, without requiring manual steps.

## Error Handling

### XDP Program (Kernel Side)

| Error Condition | Behavior |
|----------------|----------|
| Packet too small for Ethernet/IP/UDP | `XDP_PASS` |
| Not IPv4 or not UDP | `XDP_PASS` |
| Not destination port 53 | `XDP_PASS` |
| DNS parse failure (truncated, malformed) | `XDP_PASS` |
| QDCOUNT != 1 | `XDP_PASS` |
| BPF map lookup returns NULL | `XDP_PASS` |
| Cache entry expired | `XDP_PASS` |
| `bpf_xdp_adjust_tail()` fails (response too large for frame) | `XDP_PASS` |
| Checksum calculation overflow | `XDP_PASS` (defensive check) |

**Principle:** Any uncertainty → `XDP_PASS`. The userspace pipeline is the safe fallback.

### Userspace Side

| Error Condition | Behavior |
|----------------|----------|
| eBPF ELF not found / corrupt | Log error, skip XDP init, continue without XDP |
| Interface not found (auto-detect fails, no env/config) | Log error, skip XDP init |
| XDP attach fails (permission denied, kernel too old) | Log warning, skip XDP init |
| BPF map open/pin fails | Log warning, skip XDP init |
| `cache_response()` write fails | Log warning, response still sent to client normally |
| Map schema mismatch (old pinned map from previous version) | Log error, delete old pin, create new map |

## Testing Strategy

### Unit Tests

- `interface.rs`: Mock `/proc/net/route` content, verify auto-detection
- `cache.rs`: Test key/value serialization round-trip
- `middleware.rs`: Test `after_request` with various `RequestOutcome` values (NoError, NXDomain, ServFail)

### Integration Tests

- Load eBPF program in a test namespace (veth pair), send UDP DNS queries, verify XDP cache hit/miss behavior
- Verify negative caching: send query for non-existent name, confirm NXDomain response is cached and served from XDP on second query
- Verify TTL expiry: write entry with short TTL, wait, confirm next query passes through to userspace

### Benchmark Targets

- XDP cache hit latency vs. userspace-only latency
- Cache write throughput (middleware `after_request` overhead)
- BPF map lookup latency at various cache sizes (1K, 16K, 64K entries)

## CI/CD: GitHub Actions Pipeline

### Overview

Every PR and push to `main` triggers two CI jobs on `ubuntu-latest` (kernel 6.x, native eBPF support):

1. **`xdp-integration-test`** — runs XDP integration tests with elevated privileges
2. **`xdp-benchmark`** — runs XDP benchmarks and posts a comparison report as a PR comment

```
PR / push to main
  │
  ├─→ xdp-integration-test
  │     ├─ sudo setup (CAP_BPF, CAP_NET_ADMIN)
  │     ├─ create veth pair in netns
  │     ├─ cargo test --features xdp --test xdp_integration
  │     └─ report pass/fail
  │
  └─→ xdp-benchmark
        ├─ cargo bench --features xdp --bench xdp_bench (PR branch)
        ├─ cargo bench --features xdp --bench xdp_bench (base branch)
        ├─ critcmp / github-action-benchmark
        └─ post comparison comment to PR
```

### Job: `xdp-integration-test`

**Why `sudo` is required:** XDP programs need `CAP_BPF` + `CAP_NET_ADMIN` to load and attach. Integration tests create a veth pair and attach the XDP program to it, which requires root.

```yaml
xdp-integration-test:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4

    - name: Install Rust toolchain
      uses: dtolnay/rust-toolchain@stable
      with:
        targets: bpfel-unknown-none

    - name: Install bpf-linker
      run: cargo install bpf-linker

    - name: Build eBPF program
      run: cd xdp && cargo build --target bpfel-unknown-none --release

    - name: Run XDP integration tests
      run: |
        sudo -E cargo test --features xdp --test xdp_integration -- --nocapture
      env:
        RUST_LOG: debug

    - name: Cleanup network namespace
      if: always()
      run: sudo ip netns delete doggy-dns-test || true
```

**Test environment setup (inside the integration test binary):**

```rust
// tests/xdp_integration/setup.rs

/// Create an isolated network namespace with a veth pair for XDP testing.
/// The test attaches the XDP program to one end and sends DNS queries from the other.
fn setup_test_netns() -> Result<TestEnv> {
    // ip netns add doggy-dns-test
    // ip link add veth0 type veth peer name veth1
    // ip link set veth1 netns doggy-dns-test
    // ip addr add 10.0.0.1/24 dev veth0
    // ip link set veth0 up
    // (inside netns) ip addr add 10.0.0.2/24 dev veth1
    // (inside netns) ip link set veth1 up
}
```

**Test cases covered:**

| Test | Description |
|------|-------------|
| `xdp_cache_hit_positive` | Send A query → miss → userspace resolves → send same query again → XDP hit, verify response matches |
| `xdp_cache_hit_negative` | Send query for non-existent name → NXDomain → cache → second query served from XDP with RCODE=3 |
| `xdp_cache_ttl_expiry` | Write entry with 1s TTL → wait 2s → verify next query is a miss |
| `xdp_cache_different_qtype` | Cache A record → query AAAA → verify miss (different qtype_class) |
| `xdp_cache_different_qclass` | Cache IN record → query CH → verify miss |
| `xdp_non_dns_passthrough` | Send packet to port 80 → verify XDP_PASS (not intercepted) |
| `xdp_malformed_dns_passthrough` | Send truncated DNS packet → verify XDP_PASS |
| `xdp_graceful_degradation` | Start with XDP enabled but no CAP_NET_ADMIN → verify fallback to userspace-only |

### Job: `xdp-benchmark`

**Benchmark comparison strategy:** Run benchmarks on both the PR branch and the base branch (`main`), then post a side-by-side comparison as a PR comment. This catches performance regressions before merge.

```yaml
xdp-benchmark:
  runs-on: ubuntu-latest
  permissions:
    contents: read
    pull-requests: write    # Required to post PR comments
  steps:
    - uses: actions/checkout@v4
      with:
        fetch-depth: 0      # Need full history to checkout base branch

    - name: Install Rust toolchain + bpf-linker
      # ... (same as integration test job)

    - name: Build eBPF program
      run: cd xdp && cargo build --target bpfel-unknown-none --release

    # --- Benchmark PR branch ---
    - name: Run benchmarks (PR)
      run: |
        sudo -E cargo bench --features xdp --bench xdp_bench -- --save-baseline pr
      # Criterion outputs to target/criterion/<bench_name>/pr/

    # --- Benchmark base branch ---
    - name: Checkout base branch
      run: git checkout origin/main

    - name: Run benchmarks (base)
      run: |
        sudo -E cargo bench --features xdp --bench xdp_bench -- --save-baseline base

    # --- Compare and report ---
    - name: Compare benchmarks
      uses: benchmark-action/github-action-benchmark@v1
      with:
        tool: cargo
        output-file-path: target/criterion/xdp_bench/base/output.txt
        github-token: ${{ secrets.GITHUB_TOKEN }}
        benchmark-repo-owner: ${{ github.repository_owner }}
        auto-push: false
        comment-on-alert: true
        alert-threshold: '110%'        # Alert if regression > 10%
        fail-on-alert: false           # Warn but don't block PR

    - name: Post comparison comment
      uses: actions/github-script@v7
      with:
        script: |
          // Use critcmp or custom script to generate markdown table
          // comparing pr vs base latency/throughput for each benchmark
          const comparison = await exec.getExecOutput('critcmp', ['base', 'pr']);
          github.rest.issues.createComment({
            issue_number: context.issue.number,
            owner: context.repo.owner,
            repo: context.repo.repo,
            body: '## XDP Benchmark Comparison\n\n```\n' + comparison.stdout + '\n```'
          });
```

**Benchmark cases covered:**

| Benchmark | Parameterization | What it measures |
|-----------|-----------------|------------------|
| `xdp_cache_hit_latency` | cache_size: [1K, 16K, 64K] | End-to-end latency of XDP cache hit (send query, receive response) |
| `xdp_cache_miss_latency` | cache_size: [1K, 16K, 64K] | Latency when XDP passes through to userspace (baseline comparison) |
| `xdp_negative_cache_hit_latency` | cache_size: [1K, 16K, 64K] | Latency of NXDomain cache hit (simpler than positive hit) |
| `bpf_map_lookup_latency` | cache_size: [1K, 16K, 64K] | Isolated BPF map lookup time (no packet processing) |
| `bpf_map_write_throughput` | batch_size: [1, 10, 100] | Cache write rate from userspace middleware |
| `xdp_vs_userspace_comparison` | N/A | Side-by-side: same query served by XDP vs. userspace pipeline |

**Expected output format (PR comment):**

```markdown
## XDP Benchmark Comparison

| Benchmark | Base (main) | PR | Change |
|-----------|-------------|----|--------|
| xdp_cache_hit_latency (1K) | 2.3 µs | 2.4 µs | +4.3% ✅ |
| xdp_cache_hit_latency (64K) | 2.8 µs | 2.9 µs | +3.6% ✅ |
| xdp_negative_cache_hit_latency | 1.9 µs | 2.0 µs | +5.3% ✅ |
| bpf_map_write_throughput | 1.2M/s | 1.1M/s | -8.3% ✅ |
| xdp_cache_miss_latency | 45.2 µs | 44.8 µs | -0.9% ✅ |

✅ No regressions detected (threshold: 10%)
```

### Job: `xdp-stress-test`

End-to-end load test comparing XDP fast-path mode against pure userspace mode. Unlike the micro-benchmarks above (which measure individual operations via criterion), this job runs the full doggy-dns server under sustained DNS query load and reports real-world throughput and latency.

**Load testing tool:** `dnsperf` (DNS-Tools from DNS-OARC) — the standard DNS benchmark utility, available via `apt install dnsperf`.

```yaml
xdp-stress-test:
  runs-on: ubuntu-latest
  permissions:
    contents: read
    pull-requests: write
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - run: cargo install bpf-linker
    - run: sudo apt-get install -y dnsperf

    - name: Build doggy-dns (with XDP)
      run: cargo build --features xdp --release

    # --- Prepare test data ---
    - name: Generate query file
      run: |
        # 1000 unique A-record queries, repeated to exercise cache
        for i in $(seq 1 1000); do
          echo "svc-${i}.nacos.default.doggy.test A"
        done > /tmp/queries.txt

    # --- Phase 1: Pure userspace mode (XDP disabled) ---
    - name: Start server (userspace-only)
      run: |
        ./target/release/doggy-dns --config config/doggy-dns-stress.toml &
        echo $! > /tmp/server.pid
      env:
        DOGGY_DNS_XDP_ENABLED: "false"

    - name: Warm up (userspace)
      run: dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 1 -t 1 -d 5

    - name: Stress test (userspace)
      run: |
        dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 10 -t 4 -d 30 \
          -o /tmp/userspace-result.json
        kill $(cat /tmp/server.pid)

    # --- Phase 2: XDP mode ---
    - name: Start server (XDP)
      run: |
        sudo -E ./target/release/doggy-dns --config config/doggy-dns-stress-xdp.toml &
        echo $! | sudo tee /tmp/server.pid
      env:
        DOGGY_DNS_XDP_ENABLED: "true"
        XDP_INTERFACE: lo

    - name: Warm up (XDP — populates BPF cache)
      run: dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 1 -t 1 -d 5

    - name: Stress test (XDP)
      run: |
        sudo dnsperf -s 127.0.0.1 -f /tmp/queries.txt -n 10 -t 4 -d 30 \
          -o /tmp/xdp-result.json
        sudo kill $(cat /tmp/server.pid)

    # --- Compare and report ---
    - name: Generate comparison report
      run: python3 scripts/xdp_stress_report.py \
        /tmp/userspace-result.json /tmp/xdp-result.json > /tmp/stress-report.md

    - name: Post PR comment
      uses: actions/github-script@v7
      with:
        script: |
          const fs = require('fs');
          const report = fs.readFileSync('/tmp/stress-report.md', 'utf8');
          github.rest.issues.createComment({
            issue_number: context.issue.number,
            owner: context.repo.owner,
            repo: context.repo.repo,
            body: report
          });
```

**Stress test parameters:**

| Parameter | Value | Description |
|-----------|-------|-------------|
| Query count | 1000 unique names | Simulates a medium Nacos service registry |
| Concurrency (`-t`) | 4 threads | Matches typical DaemonSet worker_threads |
| Repetitions (`-n`) | 10 | Each query sent 10 times (exercises cache after warm-up) |
| Duration (`-d`) | 30 seconds | Sustained load window |
| Query type | A record | Most common DNS query type |

**Report format (PR comment):**

```markdown
## XDP Stress Test Report

### Throughput

| Mode | QPS | Total Queries | Errors | Error Rate |
|------|-----|---------------|--------|------------|
| Userspace-only | 12,450 | 373,500 | 0 | 0.00% |
| XDP (cache hit) | 185,200 | 5,556,000 | 0 | 0.00% |
| **Improvement** | **14.9x** | — | — | — |

### Latency

| Mode | p50 | p95 | p99 | max |
|------|-----|-----|-----|-----|
| Userspace-only | 285 µs | 520 µs | 1.2 ms | 8.5 ms |
| XDP (cache hit) | 18 µs | 32 µs | 58 µs | 210 µs |
| **Improvement** | **15.8x** | **16.3x** | **20.7x** | **40.5x** |

### Cache Hit Rate

| Metric | Value |
|--------|-------|
| XDP cache entries | 1,000 |
| Cache hit rate (after warm-up) | 99.8% |
| Cache miss (expired/evicted) | 0.2% |

---
Test environment: ubuntu-latest (GitHub Actions), 4 vCPU, kernel 6.x
Query file: 1,000 unique A records, 4 threads, 30s duration
```

**Stress test config (`config/doggy-dns-stress-xdp.toml`):**

```toml
[server]
listen_addr = "127.0.0.1"
port = 53
worker_threads = 4

[xdp]
enabled = true
interface = "lo"
cache_size = 65536
negative_ttl = 300

[[plugins]]
kind = "Forward"
enabled = true
name = "upstream"
upstream_servers = ["8.8.8.8"]
```

**Key design decisions:**

- **Warm-up phase:** Before the timed stress test, a single pass of all queries populates the XDP cache. The actual test then measures cache-hit performance. This isolates the XDP fast-path benefit from cold-start effects.
- **Loopback interface (`lo`):** In CI, there's no physical NIC. XDP attaches to `lo` for testing. While `lo` XDP has different characteristics than a physical NIC, it still exercises the full BPF map lookup + response construction path.
- **Same binary, different config:** Both modes use the same `doggy-dns` binary — only the `[xdp] enabled` flag differs. This ensures the comparison isolates XDP's impact, not build differences.
- **`dnsperf` output parsing:** The `scripts/xdp_stress_report.py` script parses dnsperf's JSON output (`-o` flag) and generates the markdown comparison table.

### Repository Requirements

**Required GitHub Secrets / Permissions:**

| Permission | Purpose |
|-----------|---------|
| `contents: read` | Checkout code, read history |
| `pull-requests: write` | Post benchmark comparison comments |

**Required Runner Capabilities:**

- `ubuntu-latest` (kernel 6.x) — supports eBPF/XDP natively
- `sudo` access — needed for XDP program load and netns creation
- `CAP_BPF`, `CAP_NET_ADMIN` — granted via `sudo` in the workflow

**Branch Protection (recommended):**

- Require `xdp-integration-test` job to pass before merge
- Benchmark job is informational (warns on regression but does not block)

### Workflow Trigger

```yaml
on:
  push:
    branches: [main]
    paths:
      - 'crates/doggy-dns-xdp/**'
      - 'xdp/**'
      - 'crates/doggy-dns-plugin/**'    # RequestOutcome changes
      - 'crates/doggy-dns-core/**'      # Handler changes
  pull_request:
    branches: [main]
    paths:
      - 'crates/doggy-dns-xdp/**'
      - 'xdp/**'
      - 'crates/doggy-dns-plugin/**'
      - 'crates/doggy-dns-core/**'
```

The `paths` filter ensures XDP CI jobs only run when relevant files change, saving GitHub Actions minutes.

### Cost Optimization

- Benchmark job uses `--bench xdp_bench` (single bench file) instead of `--workspace` to limit runtime
- `ubuntu-latest` is used instead of larger runners — benchmarks are CPU-bound but not memory-intensive
- Consider caching `target/` directory between runs with `actions/cache` to speed up compilation
- Benchmark comparison uses `critcmp` (fast, Rust-native) instead of re-running full criterion analysis

## Required Interface Changes

### RequestOutcome Extension

The existing `RequestOutcome` struct (in `doggy-dns-plugin`) currently carries:

```rust
pub struct RequestOutcome {
    pub response_code: ResponseCode,
    pub authority: Option<String>,
    pub answers: usize,
}
```

The `XdpCacheMiddleware` needs additional data to construct the BPF cache entry. Extend `RequestOutcome` with:

```rust
pub struct RequestOutcome {
    pub response_code: ResponseCode,
    pub authority: Option<String>,
    pub answers: usize,
    // New fields for XDP cache support:
    pub response_bytes: Option<Vec<u8>>,   // Raw DNS response wire format (None if not populated)
    pub min_answer_ttl: Option<u32>,       // Minimum TTL across all answer RRs (None if no answers)
}
```

**Population responsibility:** The `DoggyDnsHandler` (in `doggy-dns-core/src/handler.rs`) populates these fields when constructing the `RequestOutcome` after `AuthorityChain::resolve()`. When XDP is disabled, these fields are `None` and carry zero overhead (no allocation).

**Alternative considered:** Passing the raw DNS response through the `Middleware::after_request()` signature separately. Rejected because it changes the trait interface for all middleware implementations. Extending `RequestOutcome` with `Option` fields is backward-compatible.

### attach_mode = "manual" Behavior

When `attach_mode = "manual"`, the doggy-dns process does NOT call `XDP_ATTACH` on the network interface. Instead:

1. The eBPF program is loaded into the kernel (creating the BPF map)
2. The BPF map is pinned to `pin_path`
3. An external tool (e.g., `bpftool prog load` or `ip link set dev eth0 xdp obj ...`) attaches the XDP program to the NIC
4. doggy-dns opens the pinned map by path and uses it for cache writes

This mode is useful when:
- The operator wants to manage XDP lifecycle independently of the doggy-dns process
- Running in an environment where the process lacks `CAP_NET_ADMIN` but can still open pinned BPF maps
- Debugging XDP program loading separately from attachment

In `manual` mode, if the pinned map does not exist at startup, doggy-dns logs an error and falls back to userspace-only mode (it does NOT create the map or attach the program).

## Future Extensions (Out of Scope for MVP)

- **XDP metrics:** Per-CPU BPF array map for hit/miss/pass counters, exported via Prometheus
- **IPv6 support:** Parse IPv6 headers in XDP program
- **Multi-queue XDP:** Attach to all NIC queues, use `XDP_FLAGS_SKB_MODE` as fallback
- **SOCKMAP acceleration:** Redirect cache-miss packets to per-CPU userspace sockets, bypass kernel socket layer
- **AF_XDP socket:** Full userspace packet processing for maximum throughput
- **Cache warm-up:** Pre-populate BPF map from Nacos service registry on startup
- **TCP/DoT/DoH XDP:** Requires different XDP program type or socket-level filtering
