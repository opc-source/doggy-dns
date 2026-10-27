# XDP Fast-Path Observability Design

**Status:** approved (2026-10-27)
**Parent spec:** `2026-10-26-xdp-fast-path-design.md` (the deferred "XDP-level observability" item)

## Overview

The XDP fast path answers cache hits entirely in kernel space, so those queries never
reach the userspace handler and are invisible to the existing `dns.queries` counter.
`XDP_DROP` after a frame resize is invisible too. Operators can therefore neither
measure the kernel hit ratio nor notice a silently misbehaving fast path.

This design adds five kernel-side counters — `hit`, `negative_hit`, `miss`,
`expired`, `drop` — stored in a per-CPU BPF array. Userspace reads the map on the
metrics export period, converts cumulative values into deltas, and feeds the existing
OTLP counter pipeline as `dns.xdp.queries{dns.xdp.result=...}`.

Counts are exact; only visibility is delayed (bounded by one poll period plus one
export period). There are no atomics, no ring buffer, and no sampling in the data path.

## Goals

- Make kernel-served responses, cache misses, expiries, and post-resize drops visible
  through the existing OTLP metrics pipeline.
- Add zero cross-CPU contention to the XDP hot path (per-CPU counters, plain increments).
- Recover cleanly from program reloads (map recreated, counters reset).
- Keep the userspace read cost negligible (tens of bytes per CPU per poll).

## Non-Goals

- Per-request dimensions for kernel-served answers (qtype, rcode, authority) — the
  kernel does not have them, and synthesizing them would mislead.
- Counting traffic that never reaches DNS parsing (non-IPv4, non-UDP, other ports).
- Cache occupancy / LRU entry counts.
- Per-packet event streaming (ring buffer / perf events) — exactly the lossy,
  high-concurrency design this spec avoids.
- TCP, DoT, DoH accounting (the XDP program only sees UDP).
- A Prometheus scrape endpoint; OTLP remains the only export path.

## Architecture

```
 XDP program (kernel)                         Userspace
 ┌──────────────────────────┐   read every   ┌──────────────────────────────┐
 │ outcome → XDP_STATS[0]   │   N seconds    │ XdpStatsReader.snapshot()    │
 │ (per-CPU XdpStats)       │ ─────────────► │   sum over CPUs              │
 └──────────────────────────┘                │   delta vs previous          │
                                             │   reset detection            │
                                             ▼                              │
                                             XdpMetrics::record(label, n)   │
                                             (dns.xdp.queries counter)      │
                                             ▼                              │
                                             PeriodicReader → OTLP/HTTP     │
                                             └──────────────────────────────┘
```

## eBPF data plane

### Map layout

`xdp/doggy-dns-xdp-ebpf/src/main.rs` adds one map:

```rust
#[map]
static XDP_STATS: PerCpuArray<XdpStats> = PerCpuArray::with_max_entries(1, 0);

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

`XdpStats` is mirrored in `crates/doggy-dns-xdp/src/types.rs` with the usual
`size_of` assertion (40 bytes) and `unsafe impl aya::Pod` under `cfg(target_os = "linux")`,
matching the `CacheValue` pattern.

Per-CPU rather than a single global array: XDP runs in softirq context and the same
CPU never runs two instances of the program concurrently, so a plain `+= 1` on the
CPU-local slot is safe. A global array would need atomic adds, and at high packet
rates every core would contend on one cache line. Per-CPU slots also mean the
userspace read never risks a torn total — at worst the per-CPU values are a few
microseconds apart.

### Exit-path refactor

`try_dns_xdp` currently returns `Result<u32, ()>` from roughly a dozen early exits.
Change it to return `Result<(u32, Option<Outcome>), ()>` and increment once in the
entry point:

```rust
enum Outcome { Hit, NegativeHit, Miss, Expired, Drop }

#[xdp]
pub fn dns_xdp(ctx: XdpContext) -> u32 {
    let (action, outcome) = match try_dns_xdp(&ctx) {
        Ok(v) => v,
        Err(()) => (xdp_action::XDP_DROP, Some(Outcome::Drop)),
    };
    if let Some(outcome) = outcome {
        record(outcome);
    }
    action
}

#[inline(always)]
fn record(outcome: Outcome) {
    let Some(stats) = XDP_STATS.get_ptr_mut(0) else { return };
    unsafe {
        match outcome {
            Outcome::Hit => (*stats).hit += 1,
            // ... one arm per field
        }
    }
}
```

One map lookup per packet, and only for packets that reach the DNS parsing stage;
every other `XDP_PASS` carries `None` and costs nothing. The change introduces no new
pointer arithmetic, so verifier risk is low, but the eBPF build in CI is still the
gate.

### Counting semantics

| Branch | Outcome |
|--------|---------|
| Entry found, not expired, rewrite and `finalize_headers` succeed, `XDP_TX` | `hit` (`FLAG_NEGATIVE` clear) / `negative_hit` (set) |
| Key not found | `miss` |
| Key found but `expires_at <= now` | `expired` |
| `Err(())` after `bpf_xdp_adjust_tail` (original packet no longer exists) | `drop` |
| Port mismatch, non-IPv4/UDP, malformed question, QR/QDCOUNT mismatch, invalid `resp_len` | no counter |

`hit`/`negative_hit` are incremented only after the response has been fully written,
so a failed rewrite is never reported as a served query.

## Userspace read path

### Reader

`crates/doggy-dns-xdp/src/stats.rs` (Linux-gated for the reader; layout and delta
logic stay cross-platform so they can be unit-tested anywhere):

```rust
pub struct XdpStatsReader { inner: PerCpuArray<MapData, XdpStats> }

impl XdpStatsReader {
    pub fn from_map(map: Map) -> Result<Self>; // PerCpuArray::try_from, like DnsCacheMap
    pub fn snapshot(&self) -> Result<XdpStats>; // get(&0, 0), sum each field over CPUs
}
```

`XdpManager::new` takes the map from the ELF and builds the reader, mirroring the
`DNS_CACHE` handling. `XdpManager::take_stats(&mut self) -> XdpStatsReader` hands it
to the poller. The map is not pinned: a reload recreates it at zero.

### Delta and reset handling

`delta_from(current, previous) -> XdpStatsDelta` is a pure function with per-field
`u64` subtraction. A field that decreased means the map was recreated (reload); the
current value becomes that field's delta and the delta carries `reset = true`.
First read against the zero default reports accumulated totals as the first delta.

### Poller lifecycle

`stats::spawn_stats_poller(reader, period, stop, sink)` spawns a Tokio task:

- `tokio::time::interval(period)` with `MissedTickBehavior::Skip`; the first tick is
  consumed immediately so the first emit lands one period after startup.
- `tokio::select!` between the tick and a `watch::Receiver<bool>` stop signal.
- Each tick: `snapshot()`, `delta_from`, call `sink(label, count)` for non-zero fields.
- On stop: one final snapshot and emit before the task returns, so the last partial
  window is not lost.

`snapshot()` failures are logged with `tracing::warn!` (first occurrence, then
`debug!` to avoid flooding) and the window is skipped with `previous` unchanged, so
no counts are lost. A reset logs once at `debug!`.

The sink is a closure so the XDP crate stays independent of the observability crate:

```rust
move |label: &'static str, count: u64| metrics.record(label, count)
```

## Export

`crates/doggy-dns-observability/src/metrics.rs` gains a second instrument built from
the same meter:

```
dns.xdp.queries   Unit: 1   Type: u64 Counter
  description: "DNS queries classified by the XDP fast path"
  attribute:   dns.xdp.result = hit | negative_hit | miss | expired | drop
```

`build_meter_provider` returns it alongside `DnsMetrics`; `Observability` stores it
and exposes `xdp_metrics() -> Option<Arc<XdpMetrics>>` (only when
`observability.metrics.enabled`).

### Why not merge into `dns.queries`

`dns.queries` is recorded in the userspace middleware with `dns.question.type`,
`network.protocol.name`, `dns.response.code`, and `dns.authority`. The kernel knows
none of these, so adding kernel counts to that instrument would create a sparse,
misleading attribute set. Keep them separate.

Operationally, total query volume is `dns.queries` (userspace) plus
`dns.xdp.queries{result=hit|negative_hit}`. Miss/expired counts are already covered
by `dns.queries` — they are the same queries falling through — so they must not be
added to the total again.

## Wiring

`src/main.rs`:

1. `init_xdp` still returns `Option<XdpManager>`; the manager is kept alive as before.
2. After init, `xdp_manager.as_mut().map(|m| m.take_stats())` yields the reader.
3. If both the reader and `telemetry.xdp_metrics()` are present, spawn the poller with
   `config.observability.metrics.interval_secs` and a `watch` stop channel; store the
   handle next to the MCP handle.
4. On shutdown: send the stop signal, await the handle with the same 2-second timeout
   pattern used for the MCP server, then continue to telemetry shutdown.

If XDP is enabled but OTLP metrics are disabled, log one `warn!` at startup stating
that fast-path statistics will not be exported.

No configuration changes: the poll period reuses the metrics interval, and reading
more often than the exporter drains buys nothing.

## Failure handling

| Failure | Behaviour |
|---------|-----------|
| `XDP_STATS` missing from the ELF | `XdpManager::new` fails; existing XDP init warn path applies |
| `take_stats` called twice | panic via `expect`, same contract as `take_cache` |
| Map snapshot syscall fails | warn once then debug, skip window, counters not lost |
| Counter reset (reload) | debug log, current value emitted as delta |
| Metrics disabled | no poller; startup warn when XDP is enabled |

## Testing

- Unit (any platform): `XdpStats` layout, `delta_from` normal path, per-field reset,
  first-read behaviour, label mapping.
- Build: `cargo build --features xdp` on Linux; eBPF program built in the XDP CI
  workflow so the verifier accepts the new map and exit path.
- Integration (root, `--ignored`, Linux): extend
  `crates/doggy-dns-xdp/tests/xdp_integration_test.rs` with a stats test that attaches
  to `lo` with a non-53 port, inserts a positive entry, a negative entry, and an
  expired entry directly through `DnsCacheMap`, sends the corresponding queries, then
  asserts `hit >= 1`, `negative_hit >= 1`, `expired >= 1`, and `miss >= 1` (lower
  bounds keep the test robust against duplicate loopback delivery).
  It also covers reload handling by feeding `delta_from` a previous snapshot larger
  than the current one and asserting the reset flag and current values.
- No dedicated metrics exporter test: `XdpMetrics::record` is a thin wrapper over
  `Counter::add`; attribute correctness is covered by construction.

## Operational notes

- Visibility lag is at most one poll period plus one OTLP export period (for example
  10s + 10s). Values are never sampled or approximated.
- Reloading the XDP program resets counters; the reset detection turns that into a
  zero delta instead of an underflow or a spike.
- Kernel hits do not appear in `dns.query.duration` or `dns.queries`; dashboards that
  want end-to-end demand must combine both instruments as described above.

## Alternatives considered

- **Global `Array` with atomic increments.** One read yields a consistent total, but
  every core contends on one cache line at high packet rates. Rejected.
- **Ring buffer / perf events per outcome.** Lossy under load and far more expensive
  in the data path; also duplicates kernel-side aggregation. Rejected.
- **Trait sink implemented by the observability crate.** Cleaner inversion, but forces
  `doggy-dns-observability` to depend on `doggy-dns-xdp` (pulling `aya`/`libc` into
  its Linux build) for a 20-line loop. Rejected; the closure sink keeps the dependency
  direction unchanged.
- **mpsc channel between poller and exporter.** One message per period does not
  justify an extra task and channel. Rejected.
