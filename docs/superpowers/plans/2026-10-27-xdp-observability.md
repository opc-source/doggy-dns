# XDP Fast-Path Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose kernel-side XDP cache outcomes (`hit`, `negative_hit`, `miss`, `expired`, `drop`) as the OTLP counter `dns.xdp.queries` using a per-CPU BPF map read periodically by userspace.

**Architecture:** The XDP program increments one CPU-local slot per classified packet through a single exit path; no atomics and no ring buffer. A Tokio task reads and sums the per-CPU map on the OTLP metrics interval, converts totals to deltas (with reload/reset detection), and feeds a `u64` counter with a low-cardinality `dns.xdp.result` attribute. The XDP crate exposes the reader and poller; the observability crate owns the instrument; `src/main.rs` wires them with a closure so no new crate dependency edge is introduced.

**Tech Stack:** Rust 2024, aya 0.14 / aya-ebpf 0.2 (per-CPU array maps), Tokio, OpenTelemetry SDK 0.33 / OTLP/HTTP, hickory-proto 0.26.

**Spec:** `docs/superpowers/specs/2026-10-27-xdp-observability-design.md`

## Global Constraints

- Rust edition 2024; `rustfmt.toml` sets `max_width = 100`.
- CI runs `cargo clippy --all-features --all-targets -- -D warnings`. Disallowed by `.clippy.toml`: `.unwrap()` outside tests, `std::time::Instant`, `std::thread::sleep`, `std::env::set_var/remove_var`. Test files that need them carry `#![allow(clippy::disallowed_methods)]`.
- Code that needs `aya` must be gated `#[cfg(target_os = "linux")]`; layout and delta math must compile on every platform.
- The eBPF program is a separate workspace at `xdp/` pinned to nightly. `crates/doggy-dns-xdp/build.rs` builds the ELF **only when the file is missing**, so delete `xdp/target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf` after editing eBPF sources.
- Metric contract is exact: instrument `dns.xdp.queries`, u64 counter, unit `1`, attribute key `dns.xdp.result` with values `hit`, `negative_hit`, `miss`, `expired`, `drop`.
- No new TOML config keys. The poll period is `observability.metrics.interval_secs.max(1)`.
- Commit trailer: `Co-authored-by: Qoder <noreply@qoder.com>`.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/doggy-dns-xdp/src/types.rs` | `XdpStats` layout shared with the eBPF program (modify) |
| `crates/doggy-dns-xdp/src/stats.rs` | Delta math (all platforms); map reader and poller (Linux) (create) |
| `crates/doggy-dns-xdp/src/lib.rs` | Export `stats` module (modify) |
| `crates/doggy-dns-xdp/src/program.rs` | Take `XDP_STATS` from the ELF, hand it to the middleware side (modify) |
| `crates/doggy-dns-xdp/tests/stats_test.rs` | Unit tests for layout, delta, emission (create) |
| `crates/doggy-dns-xdp/tests/xdp_integration_test.rs` | Root-only counter test with real traffic (modify) |
| `xdp/doggy-dns-xdp-ebpf/src/main.rs` | Map, outcome classification, single-exit recording (modify) |
| `crates/doggy-dns-observability/src/metrics.rs` | `XdpMetrics` instrument + in-memory exporter test (modify) |
| `crates/doggy-dns-observability/src/lib.rs` | Store and expose `XdpMetrics` (modify) |
| `src/main.rs` | Spawn the poller, stop it on shutdown (modify) |
| `docs/observability.md` | Document the new metric (modify) |

---

### Task 1: Shared counter layout and delta math

**Files:**
- Modify: `crates/doggy-dns-xdp/src/types.rs`
- Create: `crates/doggy-dns-xdp/src/stats.rs`
- Modify: `crates/doggy-dns-xdp/src/lib.rs`
- Test: `crates/doggy-dns-xdp/tests/stats_test.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `doggy_dns_xdp::types::XdpStats { hit, negative_hit, miss, expired, drop: u64 }`
  - `XdpStats::delta_from(&self, previous: &XdpStats) -> XdpStatsDelta`
  - `doggy_dns_xdp::stats::XdpStatsDelta { hit, negative_hit, miss, expired, drop: u64, reset: bool }`
  - `XdpStatsDelta::entries(&self) -> impl Iterator<Item = (&'static str, u64)>`

- [ ] **Step 1: Write the failing test file**

Create `crates/doggy-dns-xdp/tests/stats_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::stats::XdpStatsDelta;
use doggy_dns_xdp::types::XdpStats;

#[test]
fn layout_matches_the_ebpf_program() {
    assert_eq!(core::mem::size_of::<XdpStats>(), 40);
    assert_eq!(core::mem::align_of::<XdpStats>(), 8);
}

#[test]
fn delta_of_the_first_snapshot_is_the_snapshot() {
    let current = XdpStats {
        hit: 3,
        negative_hit: 1,
        miss: 2,
        expired: 4,
        drop: 5,
    };

    let delta = current.delta_from(&XdpStats::default());

    assert_eq!(delta.hit, 3);
    assert_eq!(delta.negative_hit, 1);
    assert_eq!(delta.miss, 2);
    assert_eq!(delta.expired, 4);
    assert_eq!(delta.drop, 5);
    assert!(!delta.reset);
}

#[test]
fn delta_subtracts_previous_counts() {
    let previous = XdpStats {
        hit: 10,
        negative_hit: 2,
        miss: 5,
        expired: 1,
        drop: 0,
    };
    let current = XdpStats {
        hit: 13,
        negative_hit: 2,
        miss: 8,
        expired: 1,
        drop: 1,
    };

    let delta = current.delta_from(&previous);

    assert_eq!(delta.hit, 3);
    assert_eq!(delta.negative_hit, 0);
    assert_eq!(delta.miss, 3);
    assert_eq!(delta.expired, 0);
    assert_eq!(delta.drop, 1);
    assert!(!delta.reset);
}

#[test]
fn delta_flags_reset_when_a_counter_goes_backwards() {
    let previous = XdpStats {
        hit: 10,
        ..Default::default()
    };
    let current = XdpStats {
        hit: 2,
        miss: 1,
        ..Default::default()
    };

    let delta = current.delta_from(&previous);

    assert!(delta.reset);
    assert_eq!(delta.hit, 2);
    assert_eq!(delta.miss, 1);
}

#[test]
fn entries_skip_zero_counters() {
    let delta = XdpStatsDelta {
        hit: 2,
        negative_hit: 0,
        miss: 1,
        expired: 0,
        drop: 0,
        reset: false,
    };

    let entries: Vec<_> = delta.entries().collect();

    assert_eq!(entries, vec![("hit", 2), ("miss", 1)]);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p doggy-dns-xdp --test stats_test`
Expected: compile error — `doggy_dns_xdp::stats` and `XdpStats` do not exist.

- [ ] **Step 3: Add the shared layout**

In `crates/doggy-dns-xdp/src/types.rs`, insert after `XdpRuntimeConfig`:

```rust
/// Per-CPU fast-path counters written by the XDP program.
///
/// Must match `xdp/doggy-dns-xdp-ebpf/src/main.rs` exactly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XdpStats {
    /// Queries answered from the cache (`XDP_TX`, positive entry).
    pub hit: u64,
    /// Queries answered from the cache with a cached NXDOMAIN.
    pub negative_hit: u64,
    /// Queries whose key was absent from the cache.
    pub miss: u64,
    /// Queries whose key was present but already expired.
    pub expired: u64,
    /// Packets dropped after the frame was resized (rewrite failure).
    pub drop: u64,
}
```

Add to the existing `const _: () = { ... };` block, next to the other size assertions:

```rust
    assert!(core::mem::size_of::<XdpStats>() == 40);
```

Add next to the other `aya::Pod` impls:

```rust
#[cfg(target_os = "linux")]
unsafe impl aya::Pod for XdpStats {}
```

- [ ] **Step 4: Add the delta module**

Create `crates/doggy-dns-xdp/src/stats.rs`:

```rust
//! XDP fast-path counters.
//!
//! The delta math compiles on every platform so it can be unit-tested without
//! root or eBPF. The map reader and poller are Linux-only.

use crate::types::XdpStats;

/// Difference between two [`XdpStats`] snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XdpStatsDelta {
    pub hit: u64,
    pub negative_hit: u64,
    pub miss: u64,
    pub expired: u64,
    pub drop: u64,
    /// Set when at least one counter went backwards, which only happens when
    /// the BPF map is recreated by a program reload.
    pub reset: bool,
}

impl XdpStatsDelta {
    /// Non-zero counters as `(dns.xdp.result label, count)` pairs.
    pub fn entries(&self) -> impl Iterator<Item = (&'static str, u64)> {
        [
            ("hit", self.hit),
            ("negative_hit", self.negative_hit),
            ("miss", self.miss),
            ("expired", self.expired),
            ("drop", self.drop),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
    }
}

impl XdpStats {
    /// Subtract a previous snapshot, handling counter resets per field.
    ///
    /// A field that went backwards is treated as a freshly recreated map: the
    /// current value becomes the delta instead of underflowing, and `reset` is
    /// set so the caller can log it.
    pub fn delta_from(&self, previous: &XdpStats) -> XdpStatsDelta {
        fn step(current: u64, previous: u64) -> (u64, bool) {
            if current >= previous {
                (current - previous, false)
            } else {
                (current, true)
            }
        }

        let (hit, hit_reset) = step(self.hit, previous.hit);
        let (negative_hit, negative_reset) = step(self.negative_hit, previous.negative_hit);
        let (miss, miss_reset) = step(self.miss, previous.miss);
        let (expired, expired_reset) = step(self.expired, previous.expired);
        let (drop, drop_reset) = step(self.drop, previous.drop);

        XdpStatsDelta {
            hit,
            negative_hit,
            miss,
            expired,
            drop,
            reset: hit_reset
                || negative_reset
                || miss_reset
                || expired_reset
                || drop_reset,
        }
    }
}
```

- [ ] **Step 5: Export the module**

In `crates/doggy-dns-xdp/src/lib.rs`, add `pub mod stats;` between `pub mod program;` and `pub mod types;`:

```rust
pub mod cache;
pub mod interface;
pub mod middleware;
#[cfg(target_os = "linux")]
pub mod program;
pub mod stats;
pub mod types;
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p doggy-dns-xdp --test stats_test`
Expected: 5 tests pass.

- [ ] **Step 7: Format and lint**

Run: `cargo fmt -p doggy-dns-xdp && cargo clippy -p doggy-dns-xdp --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/doggy-dns-xdp/src/types.rs crates/doggy-dns-xdp/src/stats.rs crates/doggy-dns-xdp/src/lib.rs crates/doggy-dns-xdp/tests/stats_test.rs
git commit -m "$(cat <<'EOF'
feat(xdp): add shared fast-path counter layout and delta math

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 2: Count outcomes in a per-CPU BPF map

**Files:**
- Modify: `xdp/doggy-dns-xdp-ebpf/src/main.rs`

**Interfaces:**
- Consumes: the layout fixed in Task 1 (`XdpStats`, 40 bytes, five `u64` fields in order).
- Produces: a BPF map named `XDP_STATS` (`BPF_MAP_TYPE_PERCPU_ARRAY`, one entry).

There is no host-runnable unit test for eBPF code. The compile step is the test that the
map definitions and Rust types are valid; the **verifier** runs when the userspace
loader loads the ELF (Task 3's integration test).

- [ ] **Step 1: Add the map and the outcome type**

In `xdp/doggy-dns-xdp-ebpf/src/main.rs`, extend the maps import:

```rust
    maps::{Array, LruHashMap, PerCpuArray},
```

Add after `XdpRuntimeConfig` in the shared-types section:

```rust
/// Per-CPU fast-path counters. Must match crates/doggy-dns-xdp/src/types.rs exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct XdpStats {
    pub hit: u64,
    pub negative_hit: u64,
    pub miss: u64,
    pub expired: u64,
    pub drop: u64,
}
```

Add after the `XDP_CONFIG` map:

```rust
/// Per-CPU counters classified at the single exit point of the program.
#[map]
static XDP_STATS: PerCpuArray<XdpStats> = PerCpuArray::with_max_entries(1, 0);
```

Add next to the constants:

```rust
/// Why a packet fetched or matched nothing in the cache.
enum Outcome {
    Hit,
    NegativeHit,
    Miss,
    Expired,
    Drop,
}
```

- [ ] **Step 2: Record once at the entry point**

Replace the current `dns_xdp`:

```rust
#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    match try_dns_xdp(&ctx) {
        Ok(action) => action,
        // Errors only happen after the frame has been resized, so the packet
        // can no longer be handed on as the original query.
        Err(_) => xdp_action::XDP_DROP,
    }
}
```

with:

```rust
#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    let (action, outcome) = match try_dns_xdp(&ctx) {
        Ok(result) => result,
        // Errors only happen after the frame has been resized, so the packet
        // can no longer be handed on as the original query.
        Err(()) => (xdp_action::XDP_DROP, Some(Outcome::Drop)),
    };
    if let Some(outcome) = outcome {
        record(outcome);
    }
    action
}

/// Increment the current CPU's counter. One map lookup per classified packet;
/// non-DNS traffic never reaches this path.
#[inline(always)]
fn record(outcome: Outcome) {
    let Some(stats) = XDP_STATS.get_ptr_mut(0) else {
        return;
    };
    // Safety: the per-CPU map value belongs to the current CPU, and XDP runs
    // with bottom halves disabled, so no other instance can write this slot.
    unsafe {
        match outcome {
            Outcome::Hit => (*stats).hit += 1,
            Outcome::NegativeHit => (*stats).negative_hit += 1,
            Outcome::Miss => (*stats).miss += 1,
            Outcome::Expired => (*stats).expired += 1,
            Outcome::Drop => (*stats).drop += 1,
        }
    }
}
```

- [ ] **Step 3: Change the parser signature and every return site**

Change the signature:

```rust
fn try_dns_xdp(ctx: &XdpContext) -> Result<(u32, Option<Outcome>), ()> {
```

Every early `return Ok(xdp_action::XDP_PASS);` becomes `return Ok((xdp_action::XDP_PASS, None));`
— these are the checks for Ethernet length/protocol, IPv4 bounds/protocol/IHL, UDP
bounds, a missing `XDP_CONFIG` entry, port mismatch, UDP length, DNS header bounds,
QR/QDCOUNT, question parsing, query containment, invalid `resp_len`, and
`bpf_xdp_adjust_tail` failure.

The two cache-decision branches become:

```rust
    // 6. Cache lookup
    let entry = match unsafe { DNS_CACHE.get(&key) } {
        Some(entry) => entry,
        None => return Ok((xdp_action::XDP_PASS, Some(Outcome::Miss))),
    };

    // 7. TTL check
    let now = unsafe { bpf_ktime_get_ns() };
    if entry.expires_at <= now {
        return Ok((xdp_action::XDP_PASS, Some(Outcome::Expired)));
    }
```

The final success return becomes:

```rust
    // 9. Swap headers for reply, fix lengths, and recompute checksums.
    finalize_headers(ctx, ip_hdr_len, udp_offset, new_dns_len)?;

    let outcome = if negative {
        Outcome::NegativeHit
    } else {
        Outcome::Hit
    };

    Ok((xdp_action::XDP_TX, Some(outcome)))
```

Leave every `Err(())` return unchanged; the entry point maps those to `Outcome::Drop`.

- [ ] **Step 4: Build the eBPF program (verifier compile)**

The userspace build caches the ELF, so remove it first:

```bash
rm -f xdp/target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf
cd xdp
cargo build -Z build-std=core,alloc --release --target bpfel-unknown-none
cargo fmt --all -- --check
```

Expected: successful compile with no formatting diff. This requires Linux, nightly,
`rust-src`, and `bpf-linker` (the project's Podman bpf environment or CI).

- [ ] **Step 5: Commit**

```bash
git add xdp/doggy-dns-xdp-ebpf/src/main.rs
git commit -m "$(cat <<'EOF'
feat(xdp): count fast-path outcomes in a per-CPU BPF map

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 3: Read the counters from userspace

**Files:**
- Modify: `crates/doggy-dns-xdp/src/stats.rs`
- Modify: `crates/doggy-dns-xdp/src/program.rs`
- Test: `crates/doggy-dns-xdp/tests/xdp_integration_test.rs`

**Interfaces:**
- Consumes: `XdpStats` (Task 1), `XDP_STATS` map (Task 2).
- Produces:
  - `doggy_dns_xdp::stats::XdpStatsReader::from_map(map: aya::maps::Map) -> anyhow::Result<Self>`
  - `XdpStatsReader::snapshot(&self) -> anyhow::Result<XdpStats>`
  - `XdpManager::take_stats(&mut self) -> XdpStatsReader`

- [ ] **Step 1: Write the failing integration test**

In `crates/doggy-dns-xdp/tests/xdp_integration_test.rs`, add inside the existing
`mod linux_tests { ... }` block:

```rust
    use doggy_dns_basic::XdpConfig;
    use doggy_dns_xdp::cache::now_ktime_ns;
    use doggy_dns_xdp::middleware::compute_cache_entry;
    use doggy_dns_xdp::XdpManager;
    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};
    use std::net::UdpSocket;
    use std::str::FromStr;
    use std::time::Duration;

    fn query_bytes(name: &str) -> Vec<u8> {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        let mut query = Query::new();
        query.set_name(Name::from_str(name).expect("valid name"));
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.to_vec().expect("serialize query")
    }

    fn response_bytes(name: &str, code: ResponseCode) -> Vec<u8> {
        let mut message = Message::new(0x1234, MessageType::Response, OpCode::Query);
        message.set_response_code(code);
        let mut query = Query::new();
        query.set_name(Name::from_str(name).expect("valid name"));
        query.set_query_type(RecordType::A);
        message.add_query(query);
        message.to_vec().expect("serialize response")
    }

    #[test]
    #[ignore] // Requires root, Linux, and XDP support on lo
    fn xdp_stats_reflect_cache_outcomes() {
        let port: u16 = 5533;
        let pin_path = format!("/sys/fs/bpf/doggy-dns-test/stats-{port}");
        let _ = std::fs::remove_file(&pin_path);

        let config = XdpConfig {
            enabled: true,
            interface: "lo".to_string(),
            cache_size: 1024,
            negative_ttl: 30,
            attach_mode: "auto".to_string(),
            pin_path: pin_path.clone(),
        };

        let mut manager = XdpManager::new(&config, port).expect("load and attach XDP");
        let mut cache = manager.take_cache();
        let stats = manager.take_stats();
        let now = now_ktime_ns();

        let hit_name = "hit.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(hit_name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            response_bytes(hit_name, ResponseCode::NoError),
            Some(60),
            30,
            now,
        );
        cache.insert(&key, &value).expect("insert positive entry");

        let nx_name = "nx.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(nx_name).expect("name"),
            1,
            1,
            ResponseCode::NXDomain,
            response_bytes(nx_name, ResponseCode::NXDomain),
            Some(60),
            30,
            now,
        );
        cache.insert(&key, &value).expect("insert negative entry");

        let expired_name = "expired.example.";
        let (key, value) = compute_cache_entry(
            &Name::from_str(expired_name).expect("name"),
            1,
            1,
            ResponseCode::NoError,
            response_bytes(expired_name, ResponseCode::NoError),
            Some(1),
            1,
            now.saturating_sub(2_000_000_000),
        );
        cache.insert(&key, &value).expect("insert expired entry");

        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok();

        for name in [hit_name, nx_name, expired_name, "miss.example."] {
            socket
                .send_to(&query_bytes(name), ("127.0.0.1", port))
                .expect("send query");
        }

        std::thread::sleep(Duration::from_millis(200));

        let snapshot = stats.snapshot().expect("read stats map");
        assert!(snapshot.hit >= 1, "hit counter: {snapshot:?}");
        assert!(snapshot.negative_hit >= 1, "negative_hit counter: {snapshot:?}");
        assert!(snapshot.expired >= 1, "expired counter: {snapshot:?}");
        assert!(snapshot.miss >= 1, "miss counter: {snapshot:?}");

        drop(manager);
        let _ = std::fs::remove_file(&pin_path);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p doggy-dns-xdp --test xdp_integration_test`
Expected: compile error — `take_stats` does not exist (the test is `#[ignore]`, so it
must still compile).

- [ ] **Step 3: Add the reader**

Append to `crates/doggy-dns-xdp/src/stats.rs`:

```rust
#[cfg(target_os = "linux")]
use anyhow::{Context as _, Result};
#[cfg(target_os = "linux")]
use aya::maps::{Map, MapData, PerCpuArray};

/// Userspace view of the per-CPU [`XdpStats`] map.
#[cfg(target_os = "linux")]
pub struct XdpStatsReader {
    inner: PerCpuArray<MapData, XdpStats>,
}

#[cfg(target_os = "linux")]
impl XdpStatsReader {
    /// Wrap the `XDP_STATS` map taken from the loaded ELF.
    pub fn from_map(map: Map) -> Result<Self> {
        Ok(Self {
            inner: PerCpuArray::try_from(map)?,
        })
    }

    /// Read every CPU slot and sum the counters.
    ///
    /// The slots are not read atomically with respect to each other, so a
    /// snapshot can lag a few microseconds behind the "current" total. Each
    /// slot only ever increases, so deltas stay non-negative.
    pub fn snapshot(&self) -> Result<XdpStats> {
        let values = self
            .inner
            .get(&0, 0)
            .context("failed to read XDP stats map")?;

        let mut total = XdpStats::default();
        for value in values.iter() {
            total.hit += value.hit;
            total.negative_hit += value.negative_hit;
            total.miss += value.miss;
            total.expired += value.expired;
            total.drop += value.drop;
        }
        Ok(total)
    }
}
```

- [ ] **Step 4: Hand the map from the manager to the caller**

In `crates/doggy-dns-xdp/src/program.rs`, add to the imports:

```rust
#[cfg(target_os = "linux")]
use crate::stats::XdpStatsReader;
```

Add a field to `XdpManager`:

```rust
    cache: Option<DnsCacheMap>,
    stats: Option<XdpStatsReader>,
    pub interface: String,
```

In `XdpManager::new`, after the `DNS_CACHE` block and before the `XDP_CONFIG` block:

```rust
        // Take the stats map and wrap it before attach so the reader is ready
        // as soon as the program starts counting.
        let stats = ebpf
            .take_map("XDP_STATS")
            .context("XDP_STATS map not found in ELF")?;
        let stats = XdpStatsReader::from_map(stats).context("failed to read XDP_STATS map")?;
```

Initialize the field in the returned struct:

```rust
        Ok(Self {
            _ebpf: ebpf,
            _link: link_id,
            cache: Some(cache),
            stats: Some(stats),
            interface: iface,
        })
```

Add next to `take_cache`:

```rust
    /// Take ownership of the [`XdpStatsReader`]. Panics (via `expect`) if called
    /// more than once, matching [`Self::take_cache`].
    pub fn take_stats(&mut self) -> XdpStatsReader {
        self.stats
            .take()
            .expect("XDP stats reader already taken or was never initialized")
    }
```

- [ ] **Step 5: Compile-check on Linux**

On Linux (CI or the bpf environment):

```bash
rm -f xdp/target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf
cargo build -p doggy-dns-xdp
```

Expected: success. On macOS this step cannot compile the reader (Linux-gated); rely on
CI or the Podman environment.

- [ ] **Step 6: Run the root-only integration test (Linux)**

```bash
sudo -E env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test \
  -- xdp_stats_reflect_cache_outcomes --ignored --nocapture
```

Expected: PASS, with all four assertions satisfied. A failure here means the verifier
rejected the new map or the program did not classify the queries as expected.

- [ ] **Step 7: Format and lint**

Run: `cargo fmt -p doggy-dns-xdp && cargo clippy -p doggy-dns-xdp --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/doggy-dns-xdp/src/stats.rs crates/doggy-dns-xdp/src/program.rs crates/doggy-dns-xdp/tests/xdp_integration_test.rs
git commit -m "$(cat <<'EOF'
feat(xdp): read fast-path counters from the BPF map

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 4: Export `dns.xdp.queries` and document it

**Files:**
- Modify: `crates/doggy-dns-observability/src/metrics.rs`
- Modify: `crates/doggy-dns-observability/src/lib.rs`
- Modify: `docs/observability.md`

**Interfaces:**
- Consumes: nothing from earlier tasks (operates purely on `&'static str` labels).
- Produces:
  - `doggy_dns_observability::XdpMetrics::record(&self, result: &'static str, count: u64)`
  - `Observability::xdp_metrics(&self) -> Option<Arc<XdpMetrics>>`
  - `metrics::build_meter_provider(...)` now returns `(SdkMeterProvider, DnsMetrics, XdpMetrics)`.

- [ ] **Step 1: Write the failing test**

In `crates/doggy-dns-observability/src/metrics.rs`, add to the existing
`#[cfg(test)] mod tests` module:

```rust
    #[test]
    fn xdp_counter_records_result_attributes() {
        use opentelemetry_sdk::metrics::InMemoryMetricExporter;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let meter = provider.meter("test");
        let metrics = XdpMetrics::new(&meter);

        metrics.record("hit", 3);
        metrics.record("miss", 0);
        provider.force_flush().expect("flush metrics");

        let exported = exporter.get_finished_metrics().expect("metrics exported");
        let metric = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "dns.xdp.queries")
            .expect("dns.xdp.queries must be exported");

        let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() else {
            panic!("dns.xdp.queries must be a u64 sum");
        };
        let points: Vec<_> = sum.data_points().collect();
        assert_eq!(points.len(), 1, "zero counts must not create a time series");
        assert_eq!(points[0].value(), 3);

        let attributes: Vec<KeyValue> = points[0].attributes().cloned().collect();
        assert_eq!(attribute(&attributes, "dns.xdp.result"), Some("hit"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p doggy-dns-observability xdp_counter_records_result_attributes`
Expected: compile error — `XdpMetrics` does not exist.

- [ ] **Step 3: Implement the instrument**

In `crates/doggy-dns-observability/src/metrics.rs`, add after the `DnsMetrics` impl:

```rust
/// Kernel-side fast-path counters produced by the XDP program.
///
/// One counter with a low-cardinality `dns.xdp.result` attribute; kernel-served
/// hits never reach [`DnsMetrics`], so keeping the instruments separate avoids a
/// sparse attribute set.
pub struct XdpMetrics {
    queries: Counter<u64>,
}

impl XdpMetrics {
    pub(crate) fn new(meter: &Meter) -> Self {
        let queries = meter
            .u64_counter("dns.xdp.queries")
            .with_description("DNS queries classified by the XDP fast path")
            .with_unit("1")
            .build();
        Self { queries }
    }

    /// Record a per-result delta. Zero counts are skipped so an idle interval
    /// does not create an empty time series.
    pub fn record(&self, result: &'static str, count: u64) {
        if count == 0 {
            return;
        }
        self.queries
            .add(count, &[KeyValue::new("dns.xdp.result", result)]);
    }
}
```

- [ ] **Step 4: Build the instrument in the meter provider**

Change `build_meter_provider`'s signature and return value:

```rust
pub(crate) fn build_meter_provider(
    config: &ObservabilityConfig,
    resource: Resource,
) -> Result<(SdkMeterProvider, DnsMetrics, XdpMetrics)> {
```

Replace the tail:

```rust
    let meter = provider.meter(doggy_dns_plugin::APP_NAME);
    let metrics = DnsMetrics::new(&meter);
    let xdp_metrics = XdpMetrics::new(&meter);
    Ok((provider, metrics, xdp_metrics))
}
```

- [ ] **Step 5: Store and expose it**

In `crates/doggy-dns-observability/src/lib.rs`:

Change the re-export:

```rust
pub use metrics::{DnsMetrics, XdpMetrics};
```

Add a field to `Observability`:

```rust
    metrics: Option<Arc<DnsMetrics>>,
    xdp: Option<Arc<XdpMetrics>>,
```

Initialize it in `Observability::init`'s struct literal:

```rust
            metrics: None,
            xdp: None,
```

Replace the metrics block in `init`:

```rust
            if config.metrics.enabled {
                let (provider, metrics, xdp_metrics) =
                    metrics::build_meter_provider(config, built_resource.clone())?;
                observability.metrics = Some(Arc::new(metrics));
                observability.xdp = Some(Arc::new(xdp_metrics));
                observability.meter_provider = Some(provider);
            }
```

Add next to `metrics_middleware`:

```rust
    /// Recorder for kernel-side XDP counters. `None` when OTLP metrics are disabled.
    pub fn xdp_metrics(&self) -> Option<Arc<XdpMetrics>> {
        self.xdp.clone()
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p doggy-dns-observability`
Expected: all tests pass, including `xdp_counter_records_result_attributes`.

- [ ] **Step 7: Document the metric**

In `docs/observability.md`, extend the metrics table:

```
| `dns.xdp.queries` | counter | `1` | `dns.xdp.result` (`hit`, `negative_hit`, `miss`, `expired`, `drop`) |
```

Then add after the paragraph that ends with "use logs/traces for those.":

```markdown
`dns.xdp.queries` exists only when `xdp.enabled = true`. Userspace reads a per-CPU BPF
map once per metrics interval, so values lag by up to one poll period plus one export
interval; they are exact, not sampled. Kernel-served answers never appear in
`dns.queries`, so total demand is `dns.queries` plus
`dns.xdp.queries{result=hit|negative_hit}`. Do not add `miss`/`expired` again — those
queries also appear in `dns.queries`. Reloading the XDP program resets the kernel
counters; a reset is reported as a zero delta, not a negative one.
```

- [ ] **Step 8: Format and lint**

Run: `cargo fmt -p doggy-dns-observability && cargo clippy -p doggy-dns-observability --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/doggy-dns-observability/src/metrics.rs crates/doggy-dns-observability/src/lib.rs docs/observability.md
git commit -m "$(cat <<'EOF'
feat(observability): export dns.xdp.queries from kernel counters

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 5: Poll the map on the metrics interval

**Files:**
- Modify: `crates/doggy-dns-xdp/src/stats.rs`
- Test: `crates/doggy-dns-xdp/tests/stats_test.rs`

**Interfaces:**
- Consumes: `XdpStatsReader::snapshot` (Task 3), `XdpStatsDelta::entries` (Task 1).
- Produces:
  - `doggy_dns_xdp::stats::emit_delta(previous: &mut XdpStats, current: XdpStats, sink: &mut impl FnMut(&'static str, u64))`
  - `doggy_dns_xdp::stats::spawn_stats_poller(reader, period, stop, sink) -> JoinHandle<()>` (Linux only)

- [ ] **Step 1: Write the failing test**

Append to `crates/doggy-dns-xdp/tests/stats_test.rs`:

```rust
#[test]
fn emit_delta_reports_only_non_zero_changes() {
    use doggy_dns_xdp::stats::emit_delta;

    let mut previous = XdpStats::default();
    let mut recorded = Vec::new();

    emit_delta(
        &mut previous,
        XdpStats {
            hit: 1,
            ..Default::default()
        },
        &mut |label, count| recorded.push((label, count)),
    );

    assert_eq!(recorded, vec![("hit", 1)]);
    assert_eq!(previous.hit, 1);

    recorded.clear();
    emit_delta(
        &mut previous,
        XdpStats {
            hit: 3,
            miss: 2,
            ..Default::default()
        },
        &mut |label, count| recorded.push((label, count)),
    );

    assert_eq!(recorded, vec![("hit", 2), ("miss", 2)]);
    assert_eq!(previous.hit, 3);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p doggy-dns-xdp --test stats_test emit_delta_reports_only_non_zero_changes`
Expected: compile error — `emit_delta` does not exist.

- [ ] **Step 3: Implement emission and the poller**

Append to `crates/doggy-dns-xdp/src/stats.rs` (the emission helper stays
cross-platform so it can be tested everywhere):

```rust
/// Apply a snapshot to the running baseline and forward non-zero deltas.
pub fn emit_delta(
    previous: &mut XdpStats,
    current: XdpStats,
    sink: &mut impl FnMut(&'static str, u64),
) {
    let delta = current.delta_from(previous);
    if delta.reset {
        tracing::debug!("XDP stats counters reset (program reloaded)");
    }
    for (label, count) in delta.entries() {
        sink(label, count);
    }
    *previous = current;
}
```

Then add the Linux-only poller:

```rust
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::sync::watch;
#[cfg(target_os = "linux")]
use tokio::task::JoinHandle;

/// Spawn the periodic map reader.
///
/// `sink` receives `(dns.xdp.result label, delta)` for every non-zero field.
/// The task performs one final read when `stop` flips to `true`, then returns,
/// so the last partial window is not lost on shutdown.
#[cfg(target_os = "linux")]
pub fn spawn_stats_poller(
    reader: XdpStatsReader,
    period: Duration,
    mut stop: watch::Receiver<bool>,
    mut sink: impl FnMut(&'static str, u64) + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut previous = XdpStats::default();
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; consume it so the first emit lands
        // one full period after startup.
        ticker.tick().await;

        let mut warned = false;
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = stop.changed() => break,
            }

            match reader.snapshot() {
                Ok(current) => {
                    warned = false;
                    emit_delta(&mut previous, current, &mut sink);
                }
                Err(error) => {
                    // Keep the previous baseline so the next successful read
                    // still reports everything that happened in between.
                    if warned {
                        tracing::debug!(%error, "failed to read XDP stats map; skipping window");
                    } else {
                        tracing::warn!(%error, "failed to read XDP stats map; skipping window");
                        warned = true;
                    }
                }
            }
        }

        if let Ok(current) = reader.snapshot() {
            emit_delta(&mut previous, current, &mut sink);
        }
    })
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p doggy-dns-xdp --test stats_test`
Expected: 6 tests pass.

- [ ] **Step 5: Compile-check the Linux poller**

On Linux: `cargo build -p doggy-dns-xdp`
Expected: success.

- [ ] **Step 6: Format and lint**

Run: `cargo fmt -p doggy-dns-xdp && cargo clippy -p doggy-dns-xdp --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/doggy-dns-xdp/src/stats.rs crates/doggy-dns-xdp/tests/stats_test.rs
git commit -m "$(cat <<'EOF'
feat(xdp): poll fast-path counters on the metrics interval

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 6: Wire the poller into the server lifecycle

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `XdpManager::take_stats` (Task 3), `spawn_stats_poller` (Task 5), `Observability::xdp_metrics` (Task 4).
- Produces: no public API; the binary starts and stops the poller.

- [ ] **Step 1: Take the reader and start the poller**

In `src/main.rs`, replace the XDP init line:

```rust
    // Initialize XDP fast-path middleware if enabled (Linux only).
    // The manager must stay alive for the server lifetime — dropping it
    // detaches the XDP program from the interface.
    let _xdp_manager = init_xdp(&config, &mut middlewares);
```

with:

```rust
    // Initialize XDP fast-path middleware if enabled (Linux only).
    // The manager must stay alive for the server lifetime — dropping it
    // detaches the XDP program from the interface.
    let mut xdp_manager = init_xdp(&config, &mut middlewares);

    #[cfg(all(feature = "xdp", target_os = "linux"))]
    let xdp_stats = xdp_manager.as_mut().map(|manager| manager.take_stats());
    #[cfg(not(all(feature = "xdp", target_os = "linux")))]
    let xdp_stats: Option<()> = None;

    let xdp_task = start_xdp_stats(
        xdp_stats,
        telemetry.xdp_metrics(),
        Duration::from_secs(config.observability.metrics.interval_secs.max(1)),
    );
```

- [ ] **Step 2: Stop the poller during shutdown**

After the graceful-drain block (the `match tokio::time::timeout(shutdown_timeout, server.block_until_done())`
that ends with the `"doggy-dns shut down gracefully"` log) and before the MCP shutdown
block, insert:

```rust
    // Stop the stats poller and let it perform its final map read before the
    // telemetry providers are flushed in main().
    if let Some((stop, handle)) = xdp_task {
        let _ = stop.send(true);
        match tokio::time::timeout(Duration::from_secs(2), handle).await {
            Ok(Ok(())) => tracing::info!("xdp stats poller stopped"),
            Ok(Err(error)) => tracing::warn!(%error, "xdp stats poller failed"),
            Err(_) => tracing::warn!("xdp stats poller shutdown timed out"),
        }
    }
```

- [ ] **Step 3: Add the platform-gated helper**

At the bottom of `src/main.rs`, next to `init_xdp`:

```rust
/// Start the XDP counter poller when the fast path and OTLP metrics are both on.
///
/// Returns the stop sender and the task handle. The closure keeps the
/// observability crate independent of the XDP crate: only `main` knows both.
#[cfg(all(feature = "xdp", target_os = "linux"))]
fn start_xdp_stats(
    stats: Option<doggy_dns_xdp::stats::XdpStatsReader>,
    metrics: Option<Arc<doggy_dns_observability::XdpMetrics>>,
    period: Duration,
) -> Option<(
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
)> {
    let stats = stats?;
    let Some(metrics) = metrics else {
        tracing::warn!(
            "XDP fast path is enabled but OTLP metrics are disabled; \
             kernel-side counters will not be exported"
        );
        return None;
    };

    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let handle = doggy_dns_xdp::stats::spawn_stats_poller(
        stats,
        period,
        stop_rx,
        move |label, count| metrics.record(label, count),
    );
    Some((stop, handle))
}

#[cfg(not(all(feature = "xdp", target_os = "linux")))]
fn start_xdp_stats(
    _stats: Option<()>,
    _metrics: Option<Arc<doggy_dns_observability::XdpMetrics>>,
    _period: Duration,
) -> Option<(
    tokio::sync::watch::Sender<bool>,
    tokio::task::JoinHandle<()>,
)> {
    None
}
```

- [ ] **Step 4: Verify the builds**

On the current platform:

```bash
cargo check --features xdp
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
```

On Linux, additionally:

```bash
cargo build --features xdp --release
```

Expected: all succeed. On macOS only the stub helper is compiled, so the Linux wiring
is finally validated by CI's `cargo build --features xdp --release` (stress job).

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "$(cat <<'EOF'
feat: wire XDP fast-path counters into OTLP metrics

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 7: Final verification

**Files:** none (verification only).

- [ ] **Step 1: Full test and lint sweep**

```bash
cargo test --workspace
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
cd xdp && cargo fmt --all -- --check
```

Expected: everything passes. Network-dependent forward/native tests may need outbound
DNS; note any environmental skip rather than treating it as a failure.

- [ ] **Step 2: Root integration test (Linux)**

```bash
rm -f xdp/target/bpfel-unknown-none/release/doggy-dns-xdp-ebpf
cd xdp && cargo build -Z build-std=core,alloc --release --target bpfel-unknown-none
cd ..
sudo -E env "PATH=$PATH" cargo test -p doggy-dns-xdp --test xdp_integration_test \
  -- --ignored --nocapture
```

Expected: `xdp_stats_reflect_cache_outcomes` passes; the pre-existing stub tests in the
same file are untouched and still no-ops.

- [ ] **Step 3: Confirm the metric contract**

Run the server on Linux with XDP and OTLP metrics enabled, then inspect the backend
after at least two export intervals:

- `dns.xdp.queries` exists with unit `1`.
- The only attribute key is `dns.xdp.result`.
- `hit` + `negative_hit` is non-zero after traffic that was served from the cache.
- No negative deltas appear after restarting the server (reload reset path).

No commit for this task.
