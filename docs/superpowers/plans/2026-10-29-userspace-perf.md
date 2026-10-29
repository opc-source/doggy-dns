# Userspace Hot-Path Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put a comparable end-to-end throughput number in CI, then remove the provable allocation and rebuild work from the userspace DNS hot path (Nacos name resolution, release-profile budget, cached apex `SOA`, answer-record move).

**Architecture:** Five sequenced PRs. First a golden-wire test freezes today's response bytes, then a `Performance` CI job makes the userspace path measurable, then three compute changes land on top of that settled baseline: `[profile.release]` optimisation budget, cached apex `SOA`, and a rewrite of `NacosAuthority::lookup` that works on the borrowed `LowerName` instead of round-tripping through strings.

**Tech Stack:** Rust edition 2024, hickory-dns 0.26.3, tokio, dashmap 6, criterion 0.8, GitHub Actions (`rust.yml`), dnsperf, Python 3 report script.

**Spec:** `docs/superpowers/specs/2026-10-29-userspace-perf-design.md` — the plan argues from that design; read both.

## Deviation from the spec (decided here, not silently)

The spec's Workstream 1 claims a Nacos end-to-end stress scenario is possible with **no Nacos server reachable** ("every in-zone name then takes the zone-owning `NXDOMAIN` branch … No cache seeding is needed"). **That is false.** A probe run on 2026-10-29 with a Nacos-plugin config (`server_addr = "127.0.0.1:18848"`, `dns_zone = "doggy.test"`, port 5354) exited during startup:

```
Connection initialization failed after 1 attempts. The Nacos server may be unreachable.
Error: grpc request error: get result failed: the connection is not connected
```

`NacosServiceWatcher::new` calls `NamingServiceBuilder::build().await?`, and that `?` aborts boot — the server never binds a socket, so `dnsperf` would measure a dead port, not an `NXDOMAIN` path.

**Resolution used by this plan:** the end-to-end CI scenario stays on `config/doggy-dns-stress.toml` (`forward` + hickory-resolver cache), and the zone-owning negative/`SOA` path that W3b optimises gets covered by a criterion benchmark instead — Task 3 adds `handler_pipeline_nacos_nxdomain`, which today's `handler_pipeline_nxdomain` cannot measure because it uses an empty chain (`AuthorityChain::new(vec![])` never reaches `apex_soa`). Booting a Nacos container in CI is the alternative and is explicitly not taken: it adds a Java service, a gRPC port, and a readiness gate to a job that is meant to produce one number.

## Global Constraints

Applies to every task; exact values copied from the spec and `AGENTS.md`.

- No `.unwrap()` in library code — use `.expect("reason")` or `?`. Test/bench files keep `#![allow(clippy::disallowed_methods)]` at the top, which is already present in every file this plan touches.
- No `std::time::Instant`, no `std::thread::sleep`, no `std::env::set_var`/`remove_var` (`.clippy.toml`, enforced with `-D warnings`).
- `rustfmt.toml`: edition 2024, `max_width` 100. Snippets in this plan are written for clarity, not width, so run `cargo fmt --all` once after pasting a task's code and before its `--check`.
- CI gate for every task: `cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo test --workspace`.
- Observable behaviour is frozen: same response codes, same AA/RA flags, same log events and levels, same `[[plugins]]` semantics. Per-request `tracing::info!` volume is **not** changed (user decision).
- Out of scope for this plan, by user decision: `SO_REUSEPORT` fan-out, replacing hickory's UDP receive path, XDP cache mutex, new backends.
- Some tests in `tests/` make real network calls (`forward`/`native`); `cargo test --workspace` needs network.
- Commit messages are Conventional Commits, English, with `Co-authored-by: Qoder <noreply@qoder.com>`. The `(#N)` suffix is added by squash-merge, not by the committer.

## File Structure

| File | Responsibility | Tasks |
|------|----------------|-------|
| `crates/doggy-dns-core/tests/wire_golden_test.rs` | Create. Byte-level golden assertions for the four Nacos answer shapes; the tripwire for every later representation change. | 1 |
| `scripts/xdp_stress_report.py` | Modify. Add `--single` rendering so one dnsperf run produces a report; existing pair mode and callers unchanged. | 2 |
| `crates/doggy-dns-core/benches/handler_bench.rs` | Modify. Add `handler_pipeline_nacos_nxdomain` (zone-owning negative + apex SOA) — the substitute for the unreachable Nacos E2E scenario. | 3 |
| `.github/workflows/rust.yml` | Modify. Add the `performance` job: dnsperf E2E + base-vs-PR criterion for the three bench files. | 4 |
| `docs/benchmarks/userspace-baseline.md` | Create. Committed baseline: QPS, criterion medians, exact commands, runner image, date. | 4 |
| `Cargo.toml` (root) | Modify. Add `[profile.release]`. | 5 |
| `crates/doggy-dns-nacos/src/authority.rs` | Modify. Precomputed apex `SOA`; `zone_of` membership; label-count apex test; single key build; records under the `DashMap` guard. | 6, 10, 11 |
| `crates/doggy-dns-plugin/src/authority_chain.rs` | Modify. `resolve(&LowerName, …)`; `OnceLock` apex-SOA cache. | 7, 8 |
| `crates/doggy-dns-core/src/handler.rs` | Modify. Pass the borrowed `LowerName`; move answer records out of `Section`. | 8, 12 |
| `crates/doggy-dns-nacos/src/watcher.rs` | Modify. `ServiceKey(String)` composite key; `CachedInstance.ip: IpAddr`. | 9, 11 |
| `crates/doggy-dns-nacos/src/mapping.rs` | Modify. Replace `is_in_zone`/`parse_dns_name`/`DnsNameParts` with `service_key_from_name`; `to_dns_records` over `CachedInstance`. | 10, 11 |
| `crates/doggy-dns-nacos/tests/alloc_guard_test.rs` | Create. Counting `#[global_allocator]` asserting allocation counts on the hit and miss paths. | 13 |
| `crates/doggy-dns-nacos/tests/{authority_test,mapping_test,watcher_test}.rs`, `crates/doggy-dns-plugin/tests/authority_chain_test.rs`, `crates/doggy-dns-core/tests/handler_test.rs`, `tests/integration_test.rs`, `crates/doggy-dns-{plugin,nacos}/benches/*.rs`, `crates/doggy-dns-core/benches/handler_bench.rs` | Modify mechanically for the key type, `resolve` signature, and `ip` type. | 8, 9, 10, 11 |
| `AGENTS.md`, `docs/architecture.md`, `docs/development.md` | Modify. Update the Nacos flow description (`mapping::parse_dns_name` is gone), document the `performance` job and the bench-profile change. | 14 |

---

### Task 1: Golden-wire test for the Nacos answer shapes

**Files:**
- Create: `crates/doggy-dns-core/tests/wire_golden_test.rs`
- Reference only (do not modify): `crates/doggy-dns-core/tests/handler_test.rs:28-104` (request builder and `MockResponseHandler` shapes to copy)

**Interfaces:**
- Consumes: `DoggyDnsHandler::new(Vec<Arc<dyn Middleware>>, Arc<AuthorityChain>, bool)`, `NacosAuthority::new(Arc<DashMap<ServiceKey, Vec<CachedInstance>>>, &str, u32)`, `RequestHandler::handle_request`.
- Produces: a test file whose four hex constants later tasks must keep green. No production API changes.

This task runs **before** any refactor, on the current `main`, because the expected bytes are a measurement of today's behaviour.

- [ ] **Step 1: Write the capturing handler and the four cases**

Create `crates/doggy-dns-core/tests/wire_golden_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

//! Byte-level golden assertions for the answers the userspace path produces.
//!
//! The Nacos hot-path refactor (composite cache key, `zone_of` membership,
//! record move) is meant to be representation-only, so the wire bytes for a
//! fixed query set must not move. These constants were captured from `main`
//! before the refactor; if a later task changes one, that task changed the
//! response, not the expectation.

use async_trait::async_trait;
use dashmap::DashMap;
use doggy_dns_core::handler::DoggyDnsHandler;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{Name, Record, RecordType};
use hickory_proto::serialize::binary::BinEncoder;
use hickory_server::net::NetError;
use hickory_server::net::runtime::TokioTime;
use hickory_server::net::xfer::Protocol;
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::MessageResponse;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// ResponseHandler that serialises the response exactly as the server would and
/// keeps the raw datagram for byte comparison.
#[derive(Clone, Default)]
struct ByteCapturingHandler {
    bytes: Arc<Mutex<Vec<u8>>>,
}

#[async_trait]
impl ResponseHandler for ByteCapturingHandler {
    async fn send_response<'a>(
        &mut self,
        response: MessageResponse<
            '_,
            'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
            impl Iterator<Item = &'a Record> + Send + 'a,
        >,
    ) -> Result<ResponseInfo, NetError> {
        let mut bytes = Vec::new();
        let mut encoder = BinEncoder::new(&mut bytes);
        encoder.set_max_size(u16::MAX);
        let info = response
            .destructive_emit(&mut encoder)
            .expect("serialize test response");
        *self.bytes.lock().unwrap() = bytes;
        Ok(info)
    }
}

fn make_request(domain: &str, rtype: RecordType) -> Request {
    let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    let mut query = Query::new();
    query.set_name(Name::from_str(domain).unwrap());
    query.set_query_type(rtype);
    message.add_query(query);
    let bytes = message.to_vec().unwrap();
    let src = SocketAddr::from((Ipv4Addr::LOCALHOST, 12345));
    Request::from_bytes(bytes, src, Protocol::Udp).unwrap()
}

/// One **A-only** instance under `my-svc.default_group.public`.
///
/// v4-only on purpose: the NODATA case below queries AAAA, and an instance with a
/// v6 address would make that query an *answer* (NoError, ANCOUNT=1, no SOA) rather
/// than the `records.is_empty()` NODATA the spec's four-shape set requires. The
/// family-filtering branch this pins is the one Task 11 rewires.
fn nacos_handler() -> (DoggyDnsHandler, ByteCapturingHandler) {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "my-svc".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "10.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    (
        DoggyDnsHandler::new(vec![], chain, false),
        ByteCapturingHandler::default(),
    )
}

async fn wire(domain: &str, rtype: RecordType) -> Vec<u8> {
    let (handler, capture) = nacos_handler();
    let request = make_request(domain, rtype);
    handler
        .handle_request::<_, TokioTime>(&request, capture.clone())
        .await;
    capture.bytes.lock().unwrap().clone()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Assert the captured datagram, printing it so a first run can be recorded.
fn expect_bytes(bytes: &[u8], expected: &str, case: &str) {
    if expected.is_empty() {
        panic!("{case}: record this hex and paste it in:\n{}", hex(bytes));
    }
    assert_eq!(hex(bytes), expected, "{case}: wire bytes moved");
}
```

- [ ] **Step 2: Add the four test bodies with empty expectations, then record the constants**

Append to the same file (four `const`s of `""` first, so the panics print them):

```rust
const NACOS_HIT: &str = "";
const NACOS_NODATA: &str = "";
const NACOS_NXDOMAIN: &str = "";
const NACOS_APEX_SOA: &str = "";

#[tokio::test]
async fn nacos_hit_wire_is_stable() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A).await;
    expect_bytes(&bytes, NACOS_HIT, "nacos hit");
}

#[tokio::test]
async fn nacos_nodata_wire_is_stable() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::AAAA).await;
    expect_bytes(&bytes, NACOS_NODATA, "nacos nodata");
}

#[tokio::test]
async fn nacos_nxdomain_wire_is_stable() {
    let bytes = wire("unknown.default_group.public.nacos.local.", RecordType::A).await;
    expect_bytes(&bytes, NACOS_NXDOMAIN, "nacos nxdomain");
}

#[tokio::test]
async fn nacos_apex_soa_wire_is_stable() {
    let bytes = wire("nacos.local.", RecordType::SOA).await;
    expect_bytes(&bytes, NACOS_APEX_SOA, "nacos apex soa");
}
```

Run each case once to capture its bytes:

```bash
cargo test -p doggy-dns-core --test wire_golden_test -- nacos_hit --nocapture 2>&1 | grep -A1 'record this hex'
```

Do the same for the other three, then paste each printed hex into its `const`. Every case must also be sanity-checked once while you are here: the hit must carry one answer, `NODATA`/`NXDOMAIN` must carry the apex `SOA` in the authority section. Confirm with:

```bash
cargo test -p doggy-dns-core --test wire_golden_test
```

Expected: `4 passed`. If two runs of the same case print different hex (name-compression or ordering that depends on hash iteration), stop: that means the answer order is not deterministic, and the refactor must fix that before a golden test can exist. Record which case moved.

- [ ] **Step 3: Replace the recording helper with plain assertions**

`expect_bytes` existed only to print. Delete it and the four `if expected.is_empty()` lines, and assert directly:

```rust
#[tokio::test]
async fn nacos_hit_wire_is_stable() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A).await;
    assert_eq!(hex(&bytes), NACOS_HIT, "nacos hit: wire bytes moved");
}
```

(Repeat for `NACOS_NODATA`, `NACOS_NXDOMAIN`, `NACOS_APEX_SOA` with their own case names.) Also assert the parsed shape once, so a byte change is readable rather than opaque:

```rust
#[tokio::test]
async fn nacos_hit_answer_name_is_lowercased_on_the_wire() {
    let bytes = wire("my-svc.DEFAULT_GROUP.public.nacos.local.", RecordType::A).await;
    let message = Message::from_vec(&bytes).expect("parse captured response");
    assert_eq!(message.answers().len(), 1, "A query returns only the A record");
    assert!(message.metadata.authoritative, "the nacos zone owns this name");
    assert!(!message.metadata.recursion_available, "AA answers are not recursive");
}
```

- [ ] **Step 4: Verify and commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test -p doggy-dns-core --test wire_golden_test
git add crates/doggy-dns-core/tests/wire_golden_test.rs
git commit -m "$(cat <<'EOF'
test(core): freeze nacos answer bytes before the hot-path refactor

The Nacos resolution rewrite is meant to be representation-only, so the
wire form of a hit, a NODATA, an in-zone NXDOMAIN and the apex SOA needs
a guard that a timing benchmark cannot provide. Capture the datagrams
from main and assert against them.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 2: `--single` rendering in the stress report script

**Files:**
- Modify: `scripts/xdp_stress_report.py`
- Test: `scripts/test_xdp_stress_report.py` (create; stdlib `unittest`, no new dependency — the repo has no Python test runner yet, and `python3 scripts/test_xdp_stress_report.py` runs it with no configuration)

**Interfaces:**
- Consumes: `parse_dnsperf(path) -> dict | None` (existing, unchanged).
- Produces: `format_single(userspace: dict | None) -> str` and a CLI that accepts `--single <file>` in addition to the existing positional pair. The XDP workflow's `python3 scripts/xdp_stress_report.py <userspace.txt> <xdp.txt>` invocation must keep working byte-for-byte.

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_xdp_stress_report.py`:

```python
import unittest
from pathlib import Path

from xdp_stress_report import format_report, format_single, parse_dnsperf

# Resolved from this file, not the cwd: the suite has to pass from the repo
# root (python3 scripts/test_xdp_stress_report.py) and from scripts/ alike.
FIXTURE = Path(__file__).parent / "tests" / "fixtures" / "userspace.txt"


class SingleMode(unittest.TestCase):
    def setUp(self):
        self.parsed = {
            "qps": 43288.123,
            "sent": 1298645,
            "completed": 1298645,
            "lost": 0,
            "latency_avg": 0.000921,
            "latency_min": 0.000102,
            "latency_max": 0.051234,
        }

    def test_parse_reads_qps_and_latency(self):
        parsed = parse_dnsperf(FIXTURE)
        self.assertEqual(parsed["qps"], 43288.123)
        self.assertEqual(parsed["lost"], 0)

    def test_single_mode_renders_one_run(self):
        report = format_single(self.parsed)
        self.assertIn("## Userspace Performance Report", report)
        self.assertIn("43,288", report)
        self.assertIn("0.0009", report)

    def test_single_mode_handles_a_failed_run(self):
        report = format_single(None)
        self.assertIn("did not produce parseable output", report)

    def test_pair_mode_is_unchanged(self):
        report = format_report(self.parsed, None)
        self.assertIn("## XDP Stress Test Report", report)
        self.assertIn("43,288", report)
        self.assertIn("| **Ratio** | **0.00x** |", report)
        self.assertIn("N/A", report)


if __name__ == "__main__":
    unittest.main()
```

Create the fixture it reads:

```bash
mkdir -p scripts/tests/fixtures
cat > scripts/tests/fixtures/userspace.txt <<'EOF'
Queries sent: 1298645
Queries completed: 1298645
Queries lost: 0
Queries per second: 43288.123
Average Latency (s): 0.000921 (min 0.000102, max 0.051234)
EOF
```

- [ ] **Step 2: Run the tests to make sure they fail**

Run from the repo root: `python3 scripts/test_xdp_stress_report.py -v`
Expected: `ImportError: cannot import name 'format_single'` (and `AttributeError` for the missing method), not a pass.

- [ ] **Step 3: Implement `format_single` and the flag**

In `scripts/xdp_stress_report.py`, insert after `format_report`:

```python
def format_single(run):
    """Render one dnsperf run. Used by the userspace performance job, where
    there is no second mode to compare against; the committed baseline in
    docs/benchmarks/userspace-baseline.md is the comparison instead."""
    if run is None:
        return (
            "## Userspace Performance Report\n\n"
            "The userspace stress run did not produce parseable dnsperf output.\n"
        )

    return f"""## Userspace Performance Report

| Metric | Value |
|--------|-------|
| Queries per second | {fmt(run['qps'])} |
| Queries sent | {run['sent']} |
| Completed | {run['completed']} |
| Lost | {run['lost']} |
| Average latency (s) | {fmt(run['latency_avg'])} |
| Min latency (s) | {fmt(run['latency_min'])} |
| Max latency (s) | {fmt(run['latency_max'])} |

> Measured with `RUST_LOG=warn` on the `forward` plugin, so per-query `info`
> logs are suppressed and this is not the default configuration's number.
> Compare against docs/benchmarks/userspace-baseline.md; shared-runner variance
> makes the absolute value meaningless on its own.
"""
```

Replace the `__main__` block (which currently requires exactly three argv entries) with:

```python
if __name__ == "__main__":
    args = sys.argv[1:]

    if args and args[0] == "--single":
        if len(args) != 2:
            print("Usage: xdp_stress_report.py --single <run.txt>", file=sys.stderr)
            sys.exit(1)
        print(format_single(parse_dnsperf(args[1])))
    elif len(args) == 2:
        print(format_report(parse_dnsperf(args[0]), parse_dnsperf(args[1])))
    else:
        print(
            "Usage: xdp_stress_report.py <userspace.txt> <xdp.txt> | "
            "xdp_stress_report.py --single <run.txt>",
            file=sys.stderr,
        )
        sys.exit(1)
```

- [ ] **Step 4: Run the tests and the two CLI modes**

```bash
python3 scripts/test_xdp_stress_report.py -v
python3 scripts/xdp_stress_report.py --single scripts/tests/fixtures/userspace.txt
python3 scripts/xdp_stress_report.py scripts/tests/fixtures/userspace.txt scripts/tests/fixtures/userspace.txt
```

Expected: 4 tests `OK`; the first command prints the single-run report; the second prints `## XDP Stress Test Report` with a `1.00x` ratio — proof the existing pair path still works for the XDP workflow.

- [ ] **Step 5: Commit**

```bash
git add scripts/xdp_stress_report.py scripts/test_xdp_stress_report.py scripts/tests/fixtures/userspace.txt
git commit -m "$(cat <<'EOF'
feat(ci): let the stress report render a single dnsperf run

The userspace performance job measures one mode, so the two-run comparison
has nothing to pair. --single renders one run and states the logging
suppression in the footer; the existing positional pair form is unchanged
so the XDP workflow keeps working.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 3: Bench the zone-owning negative path

**Files:**
- Modify: `crates/doggy-dns-core/benches/handler_bench.rs` (add one bench, extend the `criterion_group!` list)

**Interfaces:**
- Consumes: `DoggyDnsHandler::new(vec![], chain, false)`, `NacosAuthority::new`, `AuthorityChain::new` — all unchanged.
- Produces: a benchmark id `handler_pipeline_nacos_nxdomain/threads/N` that Task 4's CI job compares base-vs-PR, standing in for the Nacos E2E scenario the spec assumed was reachable.

- [ ] **Step 1: Add the bench function**

Insert between `bench_handler_pipeline_nacos_hit` and `bench_handler_pipeline_nxdomain` in `crates/doggy-dns-core/benches/handler_bench.rs`:

```rust
/// Full pipeline on an in-zone name the cache does not hold.
///
/// The existing `handler_pipeline_nxdomain` uses an empty chain, which never
/// reaches the authoritative negative branch: the chain only fetches the apex
/// SOA when the responding handler owns its zone. This case is what the
/// apex-SOA caching work measures, and the substitute for a Nacos end-to-end
/// scenario (the server aborts boot when Nacos is unreachable, so CI cannot
/// stress that path without a live Nacos).
fn bench_handler_pipeline_nacos_nxdomain(c: &mut Criterion) {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "my-svc".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "10.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = Arc::new(AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::new(authority),
    )]));
    let handler = DoggyDnsHandler::new(vec![], chain, false);
    let request = make_request("unknown-svc.DEFAULT_GROUP.public.nacos.local.");

    let mut group = c.benchmark_group("handler_pipeline_nacos_nxdomain");
    for threads in 1..=4 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
            .unwrap();
        group.bench_with_input(BenchmarkId::new("threads", threads), &threads, |b, _| {
            b.to_async(&rt).iter(|| async {
                black_box(
                    handler
                        .handle_request::<_, TokioTime>(&request, NoOpResponseHandler)
                        .await,
                )
            });
        });
    }
    group.finish();
}
```

- [ ] **Step 2: Register it**

```rust
criterion_group!(
    benches,
    bench_handler_pipeline_nacos_hit,
    bench_handler_pipeline_nacos_nxdomain,
    bench_handler_pipeline_nxdomain,
);
```

- [ ] **Step 3: Run it and record the number**

```bash
cargo bench -p doggy-dns-core --bench handler_bench -- \
  nxdomain --sample-size 20 --measurement-time 2 --warm-up-time 1
```

The filter is `nxdomain`, not a full bench id: criterion matches a substring of the full id, and
neither `handler_pipeline_nxdomain` nor `handler_pipeline_nacos_nxdomain` is a substring of the
other. `nxdomain` selects both, so the comparison below runs in one session under identical
machine conditions.

Expected: `change:`-free criterion output for `handler_pipeline_nacos_nxdomain/threads/{1,2,3,4}` **and** `handler_pipeline_nxdomain/threads/{1,2,3,4}`, and the new medians must be **strictly greater** than `handler_pipeline_nxdomain`'s (the empty-chain case skips `apex_soa` entirely). If they are equal, the bench is not reaching the negative branch — stop and check that the request name is inside `nacos.local` and carries all three labels. Record the four medians per bench id (the PR description carries them; Task 4 reports them, Task 6/7 improve them).

- [ ] **Step 4: Verify and commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo bench --no-run --workspace
git add crates/doggy-dns-core/benches/handler_bench.rs
git commit -m "$(cat <<'EOF'
test(core): bench the zone-owning negative pipeline

The empty-chain nxdomain case never fetches an apex SOA, so the most
expensive negative path had no measurement. Bench a Nacos-zone chain that
returns authoritative NXDOMAIN instead.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 4: The `performance` CI job and the committed baseline

**Files:**
- Modify: `.github/workflows/rust.yml` (add the `performance` job after `bench`)
- Modify: `.gitignore` (add `__pycache__/`; this job runs the Python suite, which drops bytecode next to `scripts/xdp_stress_report.py`)
- Create: `docs/benchmarks/userspace-baseline.md`

**Interfaces:**
- Consumes: `config/doggy-dns-stress.toml` (existing), `scripts/xdp_stress_report.py --single` (Task 2), bench ids from `handler_bench` (Task 3), `chain_bench`, `nacos_bench`.
- Produces: a green `Performance` check on every PR, `/tmp/e2e-result.txt`, `/tmp/bench-base.txt`, `/tmp/bench-pr.txt`, and a baseline file later PRs cite.

- [ ] **Step 1: Add the job**

Append to `.github/workflows/rust.yml`:

```yaml
  performance:
    name: Performance
    runs-on: ubuntu-latest
    permissions:
      contents: read
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      # The stress report script is this job's renderer, so its suite runs here
      # rather than in the lint job: a broken --single mode fails this job loudly
      # before the report step can silently emit nothing.
      - name: Report script tests
        run: python3 scripts/test_xdp_stress_report.py -v
      - name: Install dnsperf
        run: sudo apt-get update && sudo apt-get install -y dnsperf
      - name: Generate query file
        run: |
          for i in $(seq 1 1000); do
            echo "svc-${i}.nacos.default.doggy.test A"
          done > /tmp/queries.txt
      # Component benches first: they need their own release builds, and the
      # base-vs-PR dance below checks main out from under the worktree.
      # Comparison design mirrors xdp-ci.yml: base is saved, PR compares
      # against it in the same job on the same runner, so the delta is not
      # a cross-runner artefact.
      - name: Component benchmarks (base)
        if: github.event_name == 'pull_request'
        run: |
          git checkout origin/main
          rm -rf target/criterion
          cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline base | tee /tmp/bench-base.txt
          cargo bench -p doggy-dns-plugin --bench chain_bench -- --save-baseline base | tee -a /tmp/bench-base.txt
          cargo bench -p doggy-dns-nacos --bench nacos_bench -- --save-baseline base | tee -a /tmp/bench-base.txt
      - name: Checkout PR head
        if: github.event_name == 'pull_request'
        run: git checkout "$HEAD_SHA"
        env:
          HEAD_SHA: ${{ github.event.pull_request.head.sha }}
      - name: Component benchmarks (PR)
        if: github.event_name == 'pull_request'
        run: |
          cargo bench -p doggy-dns-core --bench handler_bench -- --baseline base | tee /tmp/bench-pr.txt
          cargo bench -p doggy-dns-plugin --bench chain_bench -- --baseline base | tee -a /tmp/bench-pr.txt
          cargo bench -p doggy-dns-nacos --bench nacos_bench -- --baseline base | tee -a /tmp/bench-pr.txt
      # A push to main has no PR to compare against; keep a fresh baseline.
      - name: Component benchmarks (main)
        if: github.event_name != 'pull_request'
        run: |
          rm -rf target/criterion
          cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline base | tee /tmp/bench-main.txt
          cargo bench -p doggy-dns-plugin --bench chain_bench -- --save-baseline base | tee -a /tmp/bench-main.txt
          cargo bench -p doggy-dns-nacos --bench nacos_bench -- --save-baseline base | tee -a /tmp/bench-main.txt
      # End-to-end: the pure userspace path (no XDP), forward plugin, so this
      # measures the transport that dominates the budget. RUST_LOG=warn keeps
      # the per-query info log out of the measurement; without it the job
      # measures the runner's log pipe. The nacos scenario is deliberately
      # absent: the server aborts boot when Nacos is unreachable, so a live
      # Nacos would be a job dependency rather than a config file.
      - name: End-to-end stress (userspace path)
        run: |
          cargo build --release
          RUST_LOG=warn ./target/release/doggy-dns config/doggy-dns-stress.toml &
          echo $! > /tmp/server.pid
          sleep 2
          dnsperf -s 127.0.0.1 -p 5353 -d /tmp/queries.txt -n 1 -l 5 > /dev/null 2>&1 || true
          dnsperf -s 127.0.0.1 -p 5353 -d /tmp/queries.txt -n 10 -T 4 -l 30 > /tmp/e2e-result.txt 2>&1 || true
          kill $(cat /tmp/server.pid) 2>/dev/null || echo "server had already exited"
      - name: Generate report
        run: |
          python3 scripts/xdp_stress_report.py --single /tmp/e2e-result.txt > /tmp/e2e-report.md \
            || echo "E2E report generation skipped (dnsperf produced no parseable output)" > /tmp/e2e-report.md
      - name: Write job summary
        if: always()
        run: |
          {
            echo "## Userspace Performance (report-only)"
            echo "E2E measured with \`RUST_LOG=warn\` (per-query info logs suppressed); component benches are base-vs-PR on this same runner."
            cat /tmp/e2e-report.md 2>/dev/null || echo "no e2e report"
            if [ "${{ github.event_name }}" = "pull_request" ]; then
              echo '### Component benchmarks: base (main)'
              echo '```'
              cat /tmp/bench-base.txt 2>/dev/null || echo "benchmarks did not run"
              echo '```'
              echo '### Component benchmarks: PR (change lines compare against base)'
              echo '```'
              cat /tmp/bench-pr.txt 2>/dev/null || echo "benchmarks did not run"
              echo '```'
            else
              echo '### Component benchmarks (main)'
              echo '```'
              cat /tmp/bench-main.txt 2>/dev/null || echo "benchmarks did not run"
              echo '```'
            fi
          } >> "$GITHUB_STEP_SUMMARY"
      # A push to main has no issue/PR to comment on, and fork PRs get a
      # read-only token, so only same-repo PRs can be commented on.
      - name: Post PR comment
        if: |
          github.event_name == 'pull_request' &&
          github.event.pull_request.head.repo.full_name == github.repository
        uses: actions/github-script@v7
        with:
          script: |
            const fs = require('fs');
            // Both files can legitimately be absent: an earlier step may have
            // produced nothing, and this is a report-only job. Reading them
            // unguarded would turn a missing artefact into a red check.
            const read = (p) => fs.existsSync(p)
              ? fs.readFileSync(p, 'utf8')
              : `_${p} was not produced by this run_\n`;
            const report = read('/tmp/e2e-report.md');
            const pr = read('/tmp/bench-pr.txt');
            const body = `${report}\n## Component Benchmarks (PR vs main)\n\n\`\`\`\n${pr}\n\`\`\`\n`;
            github.rest.issues.createComment({
              issue_number: context.issue.number,
              owner: context.repo.owner,
              repo: context.repo.repo,
              body
            });
```

- [ ] **Step 2: Validate the YAML locally before pushing**

```bash
python3 -c "import yaml,sys; d=yaml.safe_load(open('.github/workflows/rust.yml')); print(sorted(d['jobs'])); print(sorted(d['jobs']['performance']['steps'][0].keys()))"
```

Expected: `['bench', 'build', 'lint', 'performance']` and `['uses', 'with']`. If `yaml` is missing, `gh workflow lint` is not available offline either — then check indentation by eye against `xdp-ci.yml`, which this block copies, and let CI be the verifier.

- [ ] **Step 3: Open the PR and confirm the job agrees with itself** *(maintainer action — pushing and opening PRs is not part of task execution)*

Push and open the PR. The spec's W1 check: run the job on this PR (which changes no hot-path code) and confirm the `change:` lines sit within noise and the E2E number lands in the same order as the XDP job's historical userspace-only run (~43k QPS). That is the calibration step — without it no later delta means anything. Read the numbers off the PR's job summary.

The task loop cannot perform this step: it is a push to a shared remote plus a PR, and the repo's convention is that the maintainer integrates. Execution therefore stops at the commit, the baseline document records what could be measured locally (Step 4), and the E2E row is marked as awaiting this job's first run. Do not invent an E2E number.

- [ ] **Step 3b: Record what is measurable without CI**

Run the three bench files locally with the committed benches and take the `threads/1` and `threads/4` medians:

```bash
cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline local
cargo bench -p doggy-dns-plugin --bench chain_bench -- --save-baseline local
cargo bench -p doggy-dns-nacos --bench nacos_bench -- --save-baseline local
```

Those medians are what Step 4's component table carries, labelled with the machine they came from. `dnsperf` is not installed locally and installing system packages is not in scope, so the end-to-end row has no local measurement and states the reference number instead.

- [ ] **Step 4: Write the baseline document**

Create `docs/benchmarks/userspace-baseline.md` with the numbers Step 3b measured (substitute the real values; the shape and commands are fixed):

```markdown
# Userspace Performance Baseline

Two kinds of number live here, and they are not interchangeable:

- **Component benchmarks** are criterion medians measured locally on the machine
  named below, from the committed benches. They are the starting point later PRs
  compare against.
- **End-to-end** is measured only by the `Performance` CI job, whose base-vs-PR
  comparison happens on one runner inside one job. That job's first run lands
  with this PR, so the end-to-end row below is the reference from the XDP stress
  job's historical userspace-only run, not a measurement taken by this job.

Numbers are runner/machine specific; only same-job base-vs-PR comparisons are
meaningful.

- Date: 2026-10-29
- CI target: `ubuntu-latest` (2 vCPU shared)
- Local bench machine: <host, CPU, core count, OS>
- Binary: `cargo build --release`, feature `xdp` off
- Server config: `config/doggy-dns-stress.toml` (forward to 8.8.8.8:53,
  `worker_threads = 4`), `RUST_LOG=warn`
- Load: `dnsperf -s 127.0.0.1 -p 5353 -d /tmp/queries.txt -n 10 -T 4 -l 30`
  over 1000 names, after one 5 s warm-up pass

## End-to-end

| Metric | Value |
|--------|-------|
| Queries per second | not measured by this job yet |
| Queries lost | not measured by this job yet |
| Average latency (s) | not measured by this job yet |

Reference: **43,288 QPS** with four worker threads, from the XDP stress job's
userspace-only run recorded in `docs/superpowers/specs/2026-10-29-userspace-perf-design.md`.
The first `Performance` job on this PR replaces the table above with its own
number; if it is not the same order of magnitude, the job is measuring something
other than the transport (check the `RUST_LOG=warn` suppression and the warm-up).

## Component benchmarks (median per iteration, measured locally)

| Benchmark | threads=1 | threads=4 |
|-----------|-----------|-----------|
| `handler_pipeline_nacos_hit` | <NS> | <NS> |
| `handler_pipeline_nacos_nxdomain` | <NS> | <NS> |
| `handler_pipeline_nxdomain` | <NS> | <NS> |
| `authority_chain_nacos_hit` | <NS> | <NS> |
| `nacos_authority_lookup_hit` | <NS> | <NS> |

## Reproduce

```bash
cargo build --release
RUST_LOG=warn ./target/release/doggy-dns config/doggy-dns-stress.toml &
dnsperf -s 127.0.0.1 -p 5353 -d /tmp/queries.txt -n 10 -T 4 -l 30
cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline base
```

## Known gaps

- The Nacos **hit** path is covered only by criterion (`handler_bench`,
  `chain_bench`, `nacos_bench`), never end-to-end: `NacosServiceWatcher::new`
  propagates a connection error and aborts boot, so a Nacos stress config
  requires a live server in the job.
- The `forward` scenario's answer set is whatever `8.8.8.8` returns, so its
  QPS is partly the upstream's. It measures our transport, not our compute.
- The job is report-only: it never fails a PR on a regression, because runner
  variance is not yet characterised well enough to gate merges on it.
```

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/rust.yml .gitignore docs/benchmarks/userspace-baseline.md
git commit -m "$(cat <<'EOF'
feat(ci): measure the userspace path end to end on every PR

The bench job only ran `cargo bench --no-run`, so nothing on the main path
measured anything while the hot path was being argued about. Add a
performance job that runs dnsperf against the userspace-only config and
compares the three component bench files base-vs-PR in the same job.
Report-only, plus a committed baseline for later PRs to cite.

The nacos end-to-end scenario the design sketched is not in it: the server
aborts boot when Nacos is unreachable, so that path is covered by a new
criterion case instead.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 5: `[profile.release]` optimisation budget

**Files:**
- Modify: `Cargo.toml` (root — append the block at the END of the file, after the last `[workspace.dependencies]` entry)

Placement matters: `[workspace.dependencies]` is a table, and a new `[profile.release]` header written *inside* it (which is where "before the first comment block" lands, since the dependency entries follow that header) terminates the table. Everything after the inserted header then parses as keys of `[profile.release]`, so the remaining dependency entries are stripped and the workspace fails to resolve. A table must be appended after the previous table's key-values have ended — which, for this file, means the last line of the file.

**Interfaces:**
- Consumes nothing; produces a faster binary and a different absolute value for every criterion number, which is why it lands alone on top of Task 4's baseline.

- [ ] **Step 1: Add the profile**

```toml
# Release and bench both inherit this. `panic = "abort"` stays out: the test
# suite and the criterion benches need unwinding to isolate a failing case.
[profile.release]
lto = "fat"
codegen-units = 1
strip = "symbols"
```

- [ ] **Step 2: Measure the shift and the build-time cost**

Run this step BEFORE committing Step 1: `git stash` can only revert the profile while the
Cargo.toml edit is still uncommitted. If it prints `No local changes to save`, both baselines were
measured under the same profile and the comparison is meaningless — say so rather than reporting a
percentage. Prefer `git stash push -- Cargo.toml` so nothing else can ride along, and paste
`git status --porcelain` before the stash, after the pop, and `git stash list` after the pop into
the PR description so the sequence is auditable. A stronger check that the two runs really used
different profiles: the bench binary hashes in criterion's output differ between them.

```bash
time cargo build --release --quiet
cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline lto
git stash push -- Cargo.toml
cargo bench -p doggy-dns-core --bench handler_bench -- --save-baseline before
git stash pop
cargo bench -p doggy-dns-core --bench handler_bench -- --baseline before
```

Expected: `change:` lines for all three handler cases. Record both the percentage and the wall-clock `cargo build --release` time in the PR description — the spec accepts the build-time cost only as a trade against the win, and the number is what makes that call. If a local `sccache`/`RUSTC_WRAPPER` is configured, its cache hits distort wall-clock comparisons between two target dirs; re-measure the pair with `RUSTC_WRAPPER=""` and say which number you are quoting.

- [ ] **Step 3: Confirm the test suite still builds and passes with the profile**

```bash
cargo test --workspace --release
```

Expected: `test result: ok` for every crate (except the pre-existing network-dependent failures, if the machine has no network — state which ran).

- [ ] **Step 4: Verify and commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
git add Cargo.toml
git commit -m "$(cat <<'EOF'
build: spend the full release optimisation budget

There was no [profile.release] section, so release and bench builds ran with
lto off and codegen-units 16. Fat LTO plus a single codegen unit is the
cheapest throughput this project can buy, and it lands alone because it
shifts every benchmark number on its own.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

**PR grouping from here:** Tasks 6-7 are one PR (`perf: stop rebuilding the apex SOA per negative answer`). Tasks 8-11 and 13 are one PR (`perf(nacos): resolve names without string churn`) - the spec allows splitting it into (a) plumbing, (b) membership + key, (c) guard + parsed IPs as reviewable commits on that PR, which is how the task boundaries are drawn. Task 12 is its own PR, and Task 14 closes the round.

---

### Task 6: Build the apex SOA once in `NacosAuthority::new`

**Files:**
- Modify: `crates/doggy-dns-nacos/src/authority.rs:15-48` (struct, `new`, `soa_record`) and `:87-97` (apex branch)
- Test: `crates/doggy-dns-nacos/tests/authority_test.rs` (extend)

**Interfaces:**
- Consumes: `Name::from_ascii`, `SOA::new(mname, rname, serial, refresh, retry, expire, minimum)`, `Record::from_rdata(Name, u32, RData)`.
- Produces: `NacosAuthority { .., apex_soa: Record }` and a file-private `fn build_soa_record(zone: &Name, ttl: u32) -> Record`. `NacosAuthority::new`'s signature is unchanged.

- [ ] **Step 1: Write the failing assertions**

Append to `crates/doggy-dns-nacos/tests/authority_test.rs`. `zone_apex_soa_query_returns_soa_record` already checks the SOA's contents; these two check that the record is *stable and shared*, which is what the caching claims:

```rust
/// The apex SOA is built once. Rebuilding it per answer was the cost; equal
/// rdata across calls is what a caller can observe, so assert that plus the
/// fact that a negative answer and an apex SOA query publish the same record.
#[tokio::test]
async fn apex_soa_is_identical_across_lookups() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);
    let apex = LowerName::from(Name::from_str("nacos.local.").unwrap());

    let mut soas = Vec::new();
    for _ in 0..3 {
        match authority
            .lookup(&apex, RecordType::SOA, None, LookupOptions::default())
            .await
        {
            LookupControlFlow::Continue(Ok(lookup)) => {
                soas.push(lookup.iter().next().expect("apex SOA query returns one record").clone());
            }
            other => panic!("expected Continue(Ok) for an apex SOA query, got {other:?}"),
        }
    }

    assert!(
        soas.windows(2).all(|pair| pair[0].data == pair[1].data
            && pair[0].name == pair[1].name
            && pair[0].ttl == pair[1].ttl),
        "repeated apex SOA lookups must return the same record"
    );
}

#[tokio::test]
async fn apex_soa_matches_the_record_a_negative_answer_carries() {
    let cache = make_test_cache();
    let authority = Arc::new(NacosAuthority::new(cache, "nacos.local", 6));
    let apex = LowerName::from(Name::from_str("nacos.local.").unwrap());
    let missing =
        LowerName::from(Name::from_str("unknown.default_group.public.nacos.local.").unwrap());

    let published = match authority
        .lookup(&apex, RecordType::SOA, None, LookupOptions::default())
        .await
    {
        LookupControlFlow::Continue(Ok(lookup)) => lookup.iter().next().cloned(),
        other => panic!("expected an apex SOA, got {other:?}"),
    };

    let chain = doggy_dns_plugin::authority_chain::AuthorityChain::new(vec![(
        "nacos".to_string(),
        Arc::clone(&authority),
    )]);
    let attached = chain
        .resolve(&Name::from(apex.clone()), RecordType::A, None)
        .await
        .soa;

    assert!(
        published.is_some() && attached == published,
        "the chain must attach the same SOA the zone publishes"
    );
}
```

`LookupError` and `Arc` are already imported in that file; add `use doggy_dns_plugin::authority_chain::AuthorityChain;` to the import block instead of the fully-qualified path if you prefer - the file currently has no plugin import, so add one:

```rust
use doggy_dns_plugin::authority_chain::AuthorityChain;
```

and use `AuthorityChain::new(...)` / `chain.resolve(...)` in the test above.

- [ ] **Step 2: Run the tests to make sure they fail**

Run: `cargo test -p doggy-dns-nacos --test authority_test -- apex_soa`
Expected: a **compile failure**, not an assertion failure - `resolve` takes `&Name` today and `Record` does not implement `PartialEq` in a way that makes `attached == published` compile if `Option<Record>` is compared by identity. If it compiles and passes, Step 1 has not expressed the claim; check that the second test drives the chain (that is the half that used to rebuild the record on every call).

Note: hickory's `Record` derives `PartialEq` over name/ttl/class/data, so `attached == published` compiles. If it does not, compare field by field exactly as the first test does.

- [ ] **Step 3: Precompute the record**

In `crates/doggy-dns-nacos/src/authority.rs`, replace the struct, `new`, and `soa_record` (`:15-48`) with:

```rust
pub struct NacosAuthority {
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    dns_zone: String,
    ttl: u32,
    origin: LowerName,
    /// Apex SOA, built once. Every in-zone negative answer attaches it, and
    /// rebuilding it cost two `format!` plus two `Name::from_ascii` per query.
    apex_soa: Record,
}

impl NacosAuthority {
    pub fn new(
        cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
        dns_zone: &str,
        ttl: u32,
    ) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let zone = Name::from_ascii(&zone_name).unwrap_or_default();
        let origin = LowerName::from(zone.clone());
        let apex_soa = build_soa_record(&zone, ttl);
        Self {
            cache,
            dns_zone: dns_zone.to_string(),
            ttl,
            origin,
            apex_soa,
        }
    }
}

/// SOA for the zone apex.
///
/// The zone is synthesized from Nacos, so there is no real primary or serial:
/// the apex itself is the MNAME, `hostmaster.<zone>` the RNAME and the
/// configured record TTL doubles as the negative-caching TTL.
fn build_soa_record(zone: &Name, ttl: u32) -> Record {
    let admin = Name::from_ascii(format!("hostmaster.{zone}")).unwrap_or_default();
    let soa = SOA::new(zone.clone(), admin, 1, 3600, 600, 86400, ttl);
    Record::from_rdata(zone.clone(), ttl, RData::SOA(soa))
}
```

and change the apex branch (`:88-92`) to clone the cached record:

```rust
            return if rtype == RecordType::SOA {
                LookupControlFlow::Continue(Ok(AuthLookup::answers(
                    LookupRecords::Section(vec![self.apex_soa.clone()]),
                    None,
                )))
            } else {
```

- [ ] **Step 4: Run the tests and the golden-wire check**

```bash
cargo test -p doggy-dns-nacos --test authority_test
cargo test -p doggy-dns-core --test wire_golden_test
```

Expected: all pass, including `nacos_apex_soa_wire_is_stable` and `nacos_nxdomain_wire_is_stable` - the SOA's bytes must not move, which proves `build_soa_record` is equivalent to the old `soa_record`.

- [ ] **Step 5: Bench the negative path**

Task 5 Step 2 saved a `before` baseline. Re-run the same benches against it and record the `change:` lines:

```bash
cargo bench -p doggy-dns-core --bench handler_bench -- \
  handler_pipeline_nacos_nxdomain --baseline before
```

If the `before` baseline is gone (`cargo bench` says the baseline does not exist), rebuild it from the stashed tree before continuing - `git stash`, `cargo bench -p doggy-dns-core --bench handler_bench -- handler_pipeline_nacos_nxdomain --save-baseline before`, `git stash pop`.

Expect little movement here: the apex-SOA query only gets cheaper by one `Record` clone. The real win on the negative path arrives with Task 7, which stops the second lookup.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates/doggy-dns-nacos/src/authority.rs crates/doggy-dns-nacos/tests/authority_test.rs
git commit -m "$(cat <<'EOF'
perf(nacos): build the zone apex SOA once

Every in-zone negative answer attached the apex SOA, and the authority
rebuilt it each time from two format! calls and two Name::from_ascii.
The zone and the TTL are fixed at construction, so the record is too.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 7: Cache the apex SOA in the chain

**Files:**
- Modify: `crates/doggy-dns-plugin/src/authority_chain.rs:20-23` (`NamedHandler`), `:64-72` (the `soa` branch), `:6` (imports)
- Test: `crates/doggy-dns-plugin/tests/authority_chain_test.rs` (extend)

**Interfaces:**
- Consumes: `apex_soa(handler, request_info).await` (existing private fn), `std::sync::OnceLock`.
- Produces: `struct NamedHandler { name: String, handler: Arc<dyn ZoneHandler>, apex_soa: OnceLock<Option<Record>> }`, private to the module. `AuthorityChain::new`'s signature and `ResolveResult`'s shape are unchanged.

- [ ] **Step 1: Write the failing test**

Append to `crates/doggy-dns-plugin/tests/authority_chain_test.rs`. A counting handler is the only way to observe the fetch, so it is also the test that would have caught this cost:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};

/// Authoritative handler that counts how many times the chain looked at it.
/// An in-zone negative answer used to cost two lookups: the answer and a
/// second one for the apex SOA.
struct CountingPrimaryHandler {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ZoneHandler for CountingPrimaryHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        static ORIGIN: std::sync::LazyLock<LowerName> =
            std::sync::LazyLock::new(|| LowerName::from(Name::from_str("example.com.").unwrap()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if rtype == RecordType::SOA {
            let soa = hickory_proto::rr::Record::from_rdata(
                Name::from_str("example.com.").unwrap(),
                30,
                hickory_proto::rr::RData::SOA(hickory_proto::rr::rdata::SOA::new(
                    Name::from_str("example.com.").unwrap(),
                    Name::from_str("admin.example.com.").unwrap(),
                    1,
                    3600,
                    600,
                    86400,
                    30,
                )),
            );
            return LookupControlFlow::Continue(Ok(AuthLookup::answers(
                LookupRecords::Section(vec![soa]),
                None,
            )));
        }
        LookupControlFlow::Continue(Err(hickory_server::zone_handler::LookupError::ResponseCode(
            hickory_proto::op::ResponseCode::NXDomain,
        )))
    }
}

#[tokio::test]
async fn negative_answers_fetch_the_apex_soa_only_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(CountingPrimaryHandler {
        calls: Arc::clone(&calls),
    });
    let chain = AuthorityChain::new(vec![("counting".to_string(), handler)]);

    let first = chain
        .resolve(
            &Name::from_str("missing.example.com.").unwrap(),
            RecordType::A,
            None,
        )
        .await;
    assert!(first.soa.is_some(), "authoritative negative carries an SOA");
    let after_first = calls.load(Ordering::Relaxed);
    assert_eq!(
        after_first, 2,
        "first negative costs one answer lookup plus one SOA lookup"
    );

    let second = chain
        .resolve(
            &Name::from_str("other.example.com.").unwrap(),
            RecordType::A,
            None,
        )
        .await;
    assert!(second.soa.is_some(), "the cached SOA must still be attached");
    assert_eq!(
        calls.load(Ordering::Relaxed) - after_first,
        1,
        "second negative must cost only its own answer lookup, not two"
    );
}
```

Do **not** add a second "non-authoritative gets no SOA" test. `crates/doggy-dns-plugin/tests/authority_chain_test.rs:304-316` already has `external_negative_is_not_authoritative_and_has_no_soa`, built on `ExternalNegativeHandler` (`ZoneType::External`, defined at `:319`), asserting `result.soa.is_none()` - that is the regression guard for the `authoritative` branch, and Step 4 just re-runs it. `ErrorZoneHandler` is the wrong fixture here: it is `ZoneType::Primary` (`:70-71`), so it *does* get an SOA fetch.

**Behaviour note:** `apex_soa()` returns `Option<Record>` and the cache stores the `Option`, so a handler whose apex SOA lookup yields nothing is cached as `None` and never re-queried - where today every negative answer pays for a second lookup that can never succeed. That is deliberate and strictly better: which handlers can answer an apex SOA does not change at runtime, and `NacosAuthority` always can after Task 6.

No other import changes: the file already imports `AxfrPolicy`, `LookupOptions`, `LookupRecords`, `ZoneType` (`:7-9`), and follows the fully-qualified `hickory_server::zone_handler::LookupError` / `hickory_proto::op::ResponseCode` convention used by the fixture above (see `:92-93`). The `AtomicUsize, Ordering` import in the snippet is the only new one.

- [ ] **Step 2: Run the tests to make sure they fail**

```bash
cargo test -p doggy-dns-plugin --test authority_chain_test -- negative_answers_fetch_the_apex_soa
```

Expected: FAIL on the last block - `assertion left: 2, right: 1`, because today the second negative re-fetches the SOA and costs two lookups. The earlier `assert_eq!(after_first, 2)` passes today; it exists to pin down what one negative costs so the second assertion is measurable.

- [ ] **Step 3: Add the `OnceLock` and read through it**

In `crates/doggy-dns-plugin/src/authority_chain.rs`, change the import line and the struct:

```rust
use std::sync::{Arc, OnceLock};
```

```rust
/// Named handler entry in the chain.
struct NamedHandler {
    name: String,
    handler: Arc<dyn ZoneHandler>,
    /// Apex SOA for this handler's negative answers, fetched once.
    ///
    /// Lazily filled rather than fetched in `new`, because `new` is synchronous
    /// and has 38 call sites. Two concurrent first-timers may both fetch; the
    /// loser's `set` is ignored, which is benign and keeps this off the
    /// async-lock path. The value goes stale only if plugin TTLs ever become
    /// hot-reloadable - the chain is rebuilt with the config in that case, as
    /// it already is for `[[plugins]]` changes.
    apex_soa: OnceLock<Option<Record>>,
}
```

```rust
    pub fn new(handlers: Vec<(String, Arc<dyn ZoneHandler>)>) -> Self {
        let handlers = handlers
            .into_iter()
            .map(|(name, handler)| NamedHandler {
                name,
                handler,
                apex_soa: OnceLock::new(),
            })
            .collect();
        Self { handlers }
    }
```

and replace the `soa` computation (`:64-72`) with:

```rust
                    let soa = if authoritative
                        && matches!(
                            result,
                            LookupControlFlow::Continue(Err(_)) | LookupControlFlow::Break(Err(_))
                        ) {
                        match entry.apex_soa.get() {
                            Some(cached) => cached.clone(),
                            None => {
                                let fetched = apex_soa(entry.handler.as_ref(), request_info).await;
                                let _ = entry.apex_soa.set(fetched.clone());
                                fetched
                            }
                        }
                    } else {
                        None
                    };
```

- [ ] **Step 4: Run the tests again**

```bash
cargo test -p doggy-dns-plugin --test authority_chain_test
cargo test -p doggy-dns-nacos --test authority_test
cargo test -p doggy-dns-core --test wire_golden_test
```

Expected: all pass; `nacos_nxdomain_wire_is_stable` proves the cached SOA serialises identically.

- [ ] **Step 5: Bench and commit**

```bash
cargo bench -p doggy-dns-core --bench handler_bench -- handler_pipeline_nacos_nxdomain --baseline before
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates/doggy-dns-plugin/src/authority_chain.rs crates/doggy-dns-plugin/tests/authority_chain_test.rs
git commit -m "$(cat <<'EOF'
perf(plugin): fetch each handler's apex SOA once

An authoritative negative answer cost two async lookups: the answer and a
second one for the apex SOA the chain attaches. The record does not change
between queries, so keep it in a OnceLock on the chain entry.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 8: Pass the borrowed `LowerName` through the chain

**Files:**
- Modify: `crates/doggy-dns-plugin/src/authority_chain.rs:42-55`
- Modify: `crates/doggy-dns-core/src/handler.rs:142-157`
- Test/bench call sites (mechanical): `tests/integration_test.rs:52,79,109`, `crates/doggy-dns-plugin/tests/authority_chain_test.rs:122,152,168,210,232,257,272,293,312` plus the two `.resolve()` calls inside `negative_answers_fetch_the_apex_soa_only_once` added by Task 7, `crates/doggy-dns-plugin/benches/chain_bench.rs:26,59`, `crates/doggy-dns-core/benches/handler_bench.rs` (no change - it calls the handler, not the chain)

**Interfaces:**
- Consumes: `hickory_proto::rr::LowerName` (already imported in `authority_chain.rs`), `hickory_proto::op::LowerQuery::name() -> &LowerName`, `ZoneHandler::lookup(&self, name: &LowerName, ...)`.
- Produces: `AuthorityChain::resolve(&self, name: &LowerName, record_type: RecordType, request_info: Option<&RequestInfo<'_>>) -> ResolveResult`.

- [ ] **Step 1: Add the test that locks the case-insensitivity guarantee**

The lowercase guarantee moves from the `to_lowercase()` calls being deleted later to the `&LowerName` contract, so it needs a test before the calls go away. Append to `crates/doggy-dns-nacos/tests/authority_test.rs`:

```rust
/// A client sends the QNAME in whatever case it likes. The cache key is
/// lowercase, and the only reason it matches is that the server hands every
/// handler a `LowerName` - so this is the contract the refactor relies on.
#[tokio::test]
async fn uppercase_wire_name_resolves_from_the_lowercase_cache() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

    let name = LowerName::from(
        Name::from_str("USER-SERVICE.DEFAULT_GROUP.PUBLIC.NACOS.LOCAL.").unwrap(),
    );
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            assert_eq!(lookup.iter().count(), 2, "both seeded instances answer");
        }
        other => panic!("uppercase QNAME must still resolve, got {other:?}"),
    }
}
```

Run: `cargo test -p doggy-dns-nacos --test authority_test -- uppercase`
Expected: PASS (it holds today, because `LowerName::from` lowercases) - this is a characterisation test, and it must stay green through Tasks 10 and 11 when the `to_lowercase()` calls disappear.

- [ ] **Step 2: Change `resolve` to take `&LowerName`**

In `crates/doggy-dns-plugin/src/authority_chain.rs`, replace `:40-55`:

```rust
    /// Resolve a query by iterating the chain.
    ///
    /// Takes the already-lowercased name the server parsed: `ZoneHandler::lookup`
    /// wants a `&LowerName` anyway, so cloning and re-lowercasing here cost four
    /// allocations per request for nothing.
    /// Returns the first non-Skip result with the handler name, or Skip if all handlers skip.
    pub async fn resolve(
        &self,
        name: &LowerName,
        record_type: RecordType,
        request_info: Option<&RequestInfo<'_>>,
    ) -> ResolveResult {
        let lookup_options = LookupOptions::default();

        for entry in &self.handlers {
            let result = entry
                .handler
                .lookup(name, record_type, request_info, lookup_options)
                .await;
```

- [ ] **Step 3: Stop the handler round-tripping through `Name`**

In `crates/doggy-dns-core/src/handler.rs`, replace `:142-157` with:

```rust
            // Resolve through authority chain. `query.name()` is the `LowerName`
            // the server already lowercased once at parse time, so it goes to the
            // chain by reference; the log string is the only owned copy.
            let query_type = request_info.query.query_type();
            let query_display = request_info.query.name().to_string();
            let resolve_span = tracing::info_span!(
                "dns.resolve",
                "dns.question.name" = %query_display,
                "dns.question.type" = tracing::field::display(query_type),
                "dns.authority" = tracing::field::Empty,
                "dns.answers" = tracing::field::Empty,
            );
            let resolve_result = self
                .authority_chain
                .resolve(
                    request_info.query.name(),
                    query_type,
                    Some(&request_info),
                )
                .instrument(resolve_span.clone())
                .await;
```

`query_display` must be identical to the old `query_name.to_string()` - both render the same `LowerName` - so every log event's `query` field is unchanged.

- [ ] **Step 4: Update every test and bench call site**

Each site changes from `chain.resolve(&name, ...)` with `name: Name` to a `LowerName`. In `tests/integration_test.rs` (`:52,79,109`), change the import and the three bindings:

```rust
use hickory_proto::rr::{LowerName, Name, RecordType};
```

```rust
    let name = LowerName::from(Name::from_str("web-app.DEFAULT_GROUP.public.nacos.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
```

```rust
    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
```

```rust
    let name = LowerName::from(Name::from_str("missing.DEFAULT_GROUP.public.nacos.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
```

In `crates/doggy-dns-plugin/tests/authority_chain_test.rs`, add `LowerName` to the `hickory_proto::rr::{...}` import and wrap all eleven `let name = Name::from_str(...)` bindings the same way. In `crates/doggy-dns-plugin/benches/chain_bench.rs`, do it **outside** the timed closure so the bench keeps measuring the resolve path, not the conversion:

```rust
    let name = LowerName::from(Name::from_str("example.com.").unwrap());
```

```rust
    let name = LowerName::from(Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap());
```

and add `LowerName` to that file's `hickory_proto::rr::{...}` import.

- [ ] **Step 5: Fix any remaining compile errors by reading them, not by guessing**

```bash
cargo test --workspace --no-run 2>&1 | grep -E "^error|call sites|-->" | head -40
```

Expected at this point: no errors. If the compiler names a `resolve` call site this plan did not list, convert it the same way and note it - `AuthorityChain::new`'s 38 call sites must **not** change.

- [ ] **Step 6: Verify, bench, commit**

```bash
cargo test -p doggy-dns-nacos --test authority_test -- uppercase
cargo test -p doggy-dns-core --test wire_golden_test
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
cargo bench -p doggy-dns-plugin --bench chain_bench -- --baseline before
cargo bench -p doggy-dns-core --bench handler_bench -- --baseline before
git add -A crates Cargo.toml tests
git commit -m "$(cat <<'EOF'
perf(plugin): resolve on the borrowed lowercase name

The handler cloned the query's LowerName into a Name and the chain
lowercased it straight back into a LowerName, four allocations per request
for a value the server had already normalised. RequestInfo carries a
LowerQuery, so the reference goes through unchanged.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 9: Make the cache key one composite string

**Files:**
- Modify: `crates/doggy-dns-nacos/src/watcher.rs:10-16` (`ServiceKey`), `:50-54` (key build), plus a separator guard in `event`
- Modify (mechanically): `crates/doggy-dns-nacos/src/authority.rs:109-113`
- Test: `crates/doggy-dns-nacos/tests/watcher_test.rs:68,88,106,129,169`, `crates/doggy-dns-nacos/tests/authority_test.rs:16,62,197,237`, `crates/doggy-dns-nacos/benches/nacos_bench.rs:17`, `crates/doggy-dns-core/benches/handler_bench.rs:61`, `crates/doggy-dns-core/tests/handler_test.rs:268`, `crates/doggy-dns-plugin/tests/authority_chain_test.rs:136,190`, `crates/doggy-dns-plugin/benches/chain_bench.rs:35`, `tests/integration_test.rs:17`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct ServiceKey(String)` with `ServiceKey::new(service_name: &str, group: &str, namespace: &str) -> Self`, `pub fn as_str(&self) -> &str`, and `pub const KEY_SEPARATOR: char = '\u{1}'`. Still `Debug + Clone + PartialEq + Eq + Hash`, so `DashMap<ServiceKey, Vec<CachedInstance>>` is unchanged. The struct literal form no longer compiles - that is what finds every seed site.

- [ ] **Step 1: Write the failing tests**

Append to `crates/doggy-dns-nacos/tests/watcher_test.rs` (this is the spec's collision risk, asserted rather than argued):

```rust
/// `service.group.namespace` is ambiguous with plain dots: `a.b` in group `c`
/// looks like `a` in group `b.c`. The separator is not a DNS label byte, so the
/// two stay distinct, which is what lets the composite key replace three fields.
#[test]
fn service_key_keeps_dotted_shapes_distinct() {
    let dotted_service = ServiceKey::new("a.b", "c", "d");
    let dotted_group = ServiceKey::new("a", "b.c", "d");

    assert_eq!(dotted_service.as_str(), "a.b\u{1}c\u{1}d");
    assert_ne!(
        dotted_service, dotted_group,
        "a service name containing dots must not be re-read as a group boundary"
    );
}

/// A Nacos identifier carrying the separator could forge a boundary and alias a
/// different triple, so the listener refuses to write such a key.
#[test]
fn listener_skips_updates_that_could_alias_another_key() {
    let (listener, cache) = make_listener("b\u{1}c", "public");

    fire_event(
        &listener,
        "a",
        Some(vec![make_service_instance("10.0.0.1", 8080, true, true)]),
    );

    assert!(
        cache.is_empty(),
        "a separator inside the group must not produce a composite key"
    );
}
```

- [ ] **Step 2: Run the tests to make sure they fail**

Run: `cargo test -p doggy-dns-nacos --test watcher_test -- service_key_keeps`
Expected: compile error `no function or associated item named 'new' found for struct 'ServiceKey'`.

- [ ] **Step 3: Replace the key type**

In `crates/doggy-dns-nacos/src/watcher.rs`, replace `:10-16` with:

```rust
/// Separator inside [`ServiceKey`].
///
/// Not a byte that can appear in a DNS label anyone will register, so a service
/// name containing dots can never be re-read as a group or namespace boundary.
pub const KEY_SEPARATOR: char = '\u{1}';

/// Key for the service instance cache: `service SEPARATOR group SEPARATOR
/// namespace`, all lowercase.
///
/// One composite string instead of three fields: a lookup used to allocate the
/// three parts and then lowercase each of them, six allocations for a value the
/// caller already had in lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceKey(String);

impl ServiceKey {
    pub fn new(service_name: &str, group: &str, namespace: &str) -> Self {
        let mut key =
            String::with_capacity(service_name.len() + group.len() + namespace.len() + 2);
        key.push_str(service_name);
        key.push(KEY_SEPARATOR);
        key.push_str(group);
        key.push(KEY_SEPARATOR);
        key.push_str(namespace);
        Self(key)
    }

    /// The composite text. For logs and assertions; not used for lookup.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// True when no part contains the composite key separator, so the assembled key
/// cannot alias a different `(service, group, namespace)` triple.
fn parts_are_separable(parts: [&str; 3]) -> bool {
    parts.iter().all(|part| !part.contains(KEY_SEPARATOR))
}
```

and in `InstanceChangeListener::event` (`:45-54`) put the guard after the `last_push` stamp - the push happened, freshness is not a function of whether we could index it:

```rust
    fn event(&self, event: Arc<NamingChangeEvent>) {
        let service_name = event.service_name.clone();
        tracing::info!("nacos service changed: {}", service_name);
        self.last_push.store(unix_now_secs(), Ordering::Relaxed);

        if !parts_are_separable([&service_name, &self.group, &self.namespace]) {
            tracing::warn!(
                service_name = %service_name,
                group = %self.group,
                namespace = %self.namespace,
                "nacos identifiers contain the cache key separator; skipping this update"
            );
            return;
        }

        let key = ServiceKey::new(
            &service_name.to_lowercase(),
            &self.group.to_lowercase(),
            &self.namespace.to_lowercase(),
        );
```

The DNS side gets the same guarantee structurally: `mapping::service_key_from_name` (Task 10) refuses a label containing `KEY_SEPARATOR as u8`, so no name-derived key can carry a stray separator either.

- [ ] **Step 4: Convert every construction site**

In `crates/doggy-dns-nacos/src/authority.rs:109-113`:

```rust
        let key = ServiceKey::new(
            &parts.service_name.to_lowercase(),
            &parts.group.to_lowercase(),
            &parts.namespace.to_lowercase(),
        );
```

Then run the compiler and convert each reported site from the struct literal to `ServiceKey::new`:

```bash
cargo test --workspace --no-run 2>&1 | grep -B2 "missing field" | grep "\-\->" | sort -u
```

The replacement shape at each site:

```rust
// before
ServiceKey {
    service_name: "user-service".to_string(),
    group: "default_group".to_string(),
    namespace: "public".to_string(),
},
// after
ServiceKey::new("user-service", "default_group", "public"),
```

The full expected list (21 occurrences): `watcher_test.rs` 5, `authority_test.rs` 4, `nacos_bench.rs` 1, `handler_bench.rs` 1, `handler_test.rs` 1, `authority_chain_test.rs` 2, `chain_bench.rs` 1, `tests/integration_test.rs` 1, `authority.rs` 1, `watcher.rs` 1, `mapping.rs` 0. If the compiler reports a site outside that list, convert it the same way - a `ServiceKey` construction is a `ServiceKey` construction.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p doggy-dns-nacos
cargo test -p doggy-dns-plugin --test authority_chain_test
cargo test -p doggy-dns-core --test handler_test
cargo test -p doggy-dns-core --test wire_golden_test
```

Expected: all pass, including the two new key tests. `wire_golden_test` passing here means the composite key resolves the same records as the three-field key did.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates tests
git commit -m "$(cat <<'EOF'
perf(nacos): use one composite string as the service cache key

Three fields meant three allocations and three hashes per lookup, and the
answer path lowercased all three again on a name the server had already
normalised. A control-character separator keeps dotted service names from
being re-read as group boundaries, and the listener refuses identifiers
that would carry the separator.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 10: Zone membership and the key in one label pass

**Files:**
- Modify: `crates/doggy-dns-nacos/src/mapping.rs` (replace `is_in_zone`, `DnsNameParts`, `parse_dns_name`)
- Modify: `crates/doggy-dns-nacos/src/authority.rs:1,51-55,78-113`
- Test: `crates/doggy-dns-nacos/tests/mapping_test.rs` (replace the five `parse_*` tests), `crates/doggy-dns-nacos/benches/nacos_bench.rs` (replace the two `parse_dns_name_*` benches)

**Interfaces:**
- Consumes: `Name::zone_of(&self, zone: &Name) -> bool` (allocation-free, case-insensitive, label-aligned), `Name::iter() -> LabelIter` with `Item = &'a [u8]` and `ExactSizeIterator`, `LowerName: Deref<Target = Name>`, `ServiceKey::new`.
- Produces: `pub fn service_key_from_name(name: &LowerName, zone: &Name) -> Option<ServiceKey>` plus a file-private `fn label_text(label: &[u8]) -> Option<&str>`. `is_in_zone`, `DnsNameParts` and `parse_dns_name` are **deleted**; the crate's public surface loses them.

- [ ] **Step 1: Write the failing tests**

In `crates/doggy-dns-nacos/tests/mapping_test.rs`, delete the five `parse_*` tests (`parse_valid_dns_name`, `parse_invalid_dns_name_wrong_zone`, `parse_invalid_dns_name_too_few_labels`, `parse_zone_match_is_label_aligned`, `parse_service_name_with_dots`) and `use doggy_dns_nacos::mapping::{parse_dns_name, to_dns_records}` and replace the import and tests with:

```rust
#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::mapping::service_key_from_name;
use doggy_dns_nacos::watcher::ServiceKey;
use doggy_dns_nacos::mapping::to_dns_records;
use doggy_dns_nacos::watcher::CachedInstance;
use hickory_proto::rr::{LowerName, Name, RecordType};
use std::str::FromStr;

fn zone() -> Name {
    Name::from_str("nacos.local").unwrap()
}

fn lower(name: &str) -> LowerName {
    LowerName::from(Name::from_str(name).unwrap())
}

#[test]
fn service_key_from_valid_dns_name() {
    let key = service_key_from_name(&lower("user-service.DEFAULT_GROUP.public.nacos.local."), &zone())
        .expect("three labels above the zone");
    assert_eq!(
        key,
        ServiceKey::new("user-service", "default_group", "public")
    );
}

#[test]
fn service_key_from_dotted_service_name() {
    let key = service_key_from_name(&lower("com.example.api.DEFAULT_GROUP.public.nacos.local."), &zone())
        .expect("dots inside the service name are not boundaries");
    assert_eq!(key, ServiceKey::new("com.example.api", "default_group", "public"));
}

#[test]
fn service_key_needs_three_labels_above_the_zone() {
    assert!(service_key_from_name(&lower("user-service.nacos.local."), &zone()).is_none());
    assert!(service_key_from_name(&lower("a.b.nacos.local."), &zone()).is_none());
}

#[test]
fn service_key_of_a_name_outside_the_zone_is_none_or_wrong_but_zone_is_checked_first() {
    // `zone_of` is label-aligned, so a sibling suffix must not be treated as
    // membership; the authority turns that into Skip. Here the key builder only
    // sees the trailing labels, so assert the authority-level contract instead.
    let sibling = lower("user-service.default_group.public.evilnacos.local.");
    assert_eq!(
        service_key_from_name(&sibling, &zone()),
        Some(ServiceKey::new("user-service.default_group.public", "evilnacos", "local"))
    );
}

#[test]
fn service_key_rejects_a_label_carrying_the_separator() {
    // A 0x01 byte inside a label would otherwise forge a field boundary. The
    // label must be supplied as raw bytes: `&str` labels go through IDNA/UTS46
    // validation, which rejects or rewrites a control byte, so this is the only
    // way to get 0x01 onto a name in a test.
    let forged = Name::from_labels([
        b"a\x01b".as_slice(),
        b"default_group".as_slice(),
        b"public".as_slice(),
        b"nacos".as_slice(),
        b"local".as_slice(),
    ])
    .expect("raw labels are only length-checked (1..=63 bytes)");
    assert!(service_key_from_name(&LowerName::from(forged), &zone()).is_none());
}
```

`Name::from_labels<I, L>` takes `I: IntoIterator<Item = L>` where `L: IntoLabel`. `IntoLabel` is implemented for `&[u8]` (raw, length-checked only) and `&str` (IDNA-validated) but **not** for `&[u8; N]`, hence the `.as_slice()` on every element - a mixed array of `&[u8]` and `&[u8; N]` would not typecheck. Add `LowerName` to the test file's `hickory_proto::rr::{...}` import if it is not there yet; no `Label` import is needed.

`service_key_of_a_name_outside_the_zone_is_none_or_wrong_but_zone_is_checked_first` is the awkward one and it is worth keeping under a shorter name if you rename it: it documents that membership is **not** this function's job, which is exactly why the authority must call `zone_of` first. The order matters, so the authority-level test `skips_sibling_zone_with_shared_suffix` in `authority_test.rs` is the real guard - confirm it still passes in Step 5.

- [ ] **Step 2: Run the tests to make sure they fail**

Run: `cargo test -p doggy-dns-nacos --test mapping_test`
Expected: compile error `unresolved import doggy_dns_nacos::mapping::service_key_from_name` (and `parse_dns_name` still referenced by `nacos_bench.rs`, which is Step 6).

- [ ] **Step 3: Replace the parsing**

In `crates/doggy-dns-nacos/src/mapping.rs`, delete `is_in_zone`, `DnsNameParts` and `parse_dns_name` and put this in their place (above the existing `to_dns_records`, which Task 11 rewrites):

```rust
use crate::watcher::{ServiceKey, KEY_SEPARATOR};
use std::borrow::Cow;

/// Build the cache key for a `<service>.<group>.<namespace>.<zone>` name.
///
/// `name` is a [`LowerName`], which is what the server hands every zone handler,
/// so the labels are already lowercase and no `to_lowercase` pass runs per
/// query - the type carries the guarantee the old code got from three
/// `to_lowercase()` calls on values that were already lower.
///
/// The zone suffix is dropped from the **right**, which is what keeps dotted
/// service names (`com.example.api`) working. Membership is *not* checked here:
/// callers must run [`Name::zone_of`] first, or a sibling like
/// `evilnacos.local` is read as if it belonged to `nacos.local`.
///
/// `None` means the name has fewer than three labels above the zone or carries a
/// label that is not UTF-8 or contains the key separator. Both are names the old
/// string parser could not have matched against a Nacos service either, so the
/// authority's authoritative `NXDOMAIN` is the same answer.
pub fn service_key_from_name(name: &LowerName, zone: &Name) -> Option<ServiceKey> {
    let zone_len = zone.iter().count();
    let name_len = name.iter().count();
    if name_len < zone_len + 3 {
        return None;
    }
    let service_len = name_len - zone_len - 2;

    let mut labels = name.iter();
    // Borrowed when the service is one label, which is the common case; a
    // dotted service pays one owned join for itself and one for the key.
    let mut service: Cow<'_, str> = Cow::Borrowed("");
    for (index, label) in labels.by_ref().take(service_len).enumerate() {
        let text = label_text(label)?;
        if index == 0 {
            service = Cow::Borrowed(text);
        } else {
            service.to_mut().push('.');
            service.to_mut().push_str(text);
        }
    }
    let group = labels.next().and_then(label_text)?;
    let namespace = labels.next().and_then(label_text)?;

    Some(ServiceKey::new(&service, group, namespace))
}

/// A DNS label as text.
///
/// Rejects the composite-key separator byte so a label can never forge a
/// field boundary, and rejects non-UTF-8 (RFC 1035 binary) labels.
fn label_text(label: &[u8]) -> Option<&str> {
    if label.contains(&(KEY_SEPARATOR as u8)) {
        return None;
    }
    std::str::from_utf8(label).ok()
}
```

Add to that file's `hickory_proto::rr::{...}` import: `LowerName` (it already imports `Name, RData, Record, RecordType, rdata::{A, AAAA}`).

- [ ] **Step 4: Point the authority at labels**

In `crates/doggy-dns-nacos/src/authority.rs`, replace the import on line 1 and delete `is_zone_apex` (`:51-55`):

```rust
use crate::mapping::{service_key_from_name, to_dns_records};
```

```rust
    async fn lookup(
        &self,
        name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        // Zone membership on labels: `zone_of` compares label bytes from the
        // right and allocates nothing, where the old string test built two
        // lowercased Strings plus a `format!(".{zone}")` - and ran twice per
        // lookup, here and inside the name parser.
        if !self.origin.zone_of(name) {
            return LookupControlFlow::Skip;
        }

        // Inside the zone we are authoritative: never fall through to
        // native/forward, or internal names could leak to an upstream resolver.
        // Equal label count plus `zone_of` is name equality.
        if name.iter().count() == self.origin.iter().count() {
            return if rtype == RecordType::SOA {
                LookupControlFlow::Continue(Ok(AuthLookup::answers(
                    LookupRecords::Section(vec![self.apex_soa.clone()]),
                    None,
                )))
            } else {
                // The zone exists, so every other apex type is NODATA.
                LookupControlFlow::Continue(Err(LookupError::NameExists))
            };
        }

        let key = match service_key_from_name(name, &self.origin) {
            Some(key) => key,
            None => {
                return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                    ResponseCode::NXDomain,
                )));
            }
        };

        let instances = match self.cache.get(&key) {
            Some(entry) => entry.value().clone(),
            None => {
                return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                    ResponseCode::NXDomain,
                )));
            }
        };
```

Leave the rest of the body as it is, except that `query_name` is no longer bound at the top of the function - bind it where it is used, so `Skip` and `NODATA` never pay for it:

```rust
        let query_name = Name::from(name.clone());
        let ips: Vec<String> = instances.iter().map(|i| i.ip.clone()).collect();
        let records = to_dns_records(&query_name, &ips, self.ttl, rtype);
```

Task 11 replaces all three lines. Also delete `is_zone_apex` and the `dns_zone: String` field, and stop assigning it in `new`; `origin` is now the only zone representation, so the struct is:

```rust
pub struct NacosAuthority {
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    ttl: u32,
    origin: LowerName,
    apex_soa: Record,
}
```

`build_soa_record` (Task 6) already takes the zone as a `&Name`, so `new` becomes:

```rust
    pub fn new(
        cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
        dns_zone: &str,
        ttl: u32,
    ) -> Self {
        let zone_name = format!("{}.", dns_zone.trim_end_matches('.'));
        let zone = Name::from_ascii(&zone_name).unwrap_or_default();
        let origin = LowerName::from(zone.clone());
        let apex_soa = build_soa_record(&zone, ttl);
        Self {
            cache,
            ttl,
            origin,
            apex_soa,
        }
    }
```

The public `new` signature is unchanged, so `main.rs`, the config validation and all 20-odd test/bench constructions keep compiling.

- [ ] **Step 5: Run the Nacos tests**

```bash
cargo test -p doggy-dns-nacos
cargo test -p doggy-dns-core --test wire_golden_test
cargo test -p doggy-dns-plugin --test authority_chain_test
```

Expected: all green. Specifically `skips_wrong_zone`, `skips_sibling_zone_with_shared_suffix`, `zone_apex_returns_nodata`, `malformed_name_in_zone_returns_nxdomain`, `family_without_records_returns_nodata` and `uppercase_wire_name_resolves_from_the_lowercase_cache` (Task 8) must pass - between them they are the contract `zone_of` inherits when the string code goes away.

- [ ] **Step 6: Replace the parse benches**

In `crates/doggy-dns-nacos/benches/nacos_bench.rs`, swap the import and the two `parse_dns_name_*` functions for the key builder, so the workstream's target (`<= 0.15 us`) stays measurable:

```rust
use doggy_dns_nacos::mapping::{service_key_from_name, to_dns_records};
```

```rust
fn bench_service_key_from_name_hit(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    let name =
        LowerName::from(Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap());
    c.bench_function("service_key_from_name_hit", |b| {
        b.iter(|| black_box(service_key_from_name(&name, &zone)));
    });
}

fn bench_service_key_from_name_miss(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    // Two labels above the zone: too few to carry group and namespace.
    let name = LowerName::from(Name::from_str("my-svc.group.nacos.local.").unwrap());
    c.bench_function("service_key_from_name_miss", |b| {
        b.iter(|| black_box(service_key_from_name(&name, &zone)));
    });
}

fn bench_service_key_dotted_service(c: &mut Criterion) {
    let zone = Name::from_str("nacos.local").unwrap();
    let name =
        LowerName::from(Name::from_str("com.example.api.DEFAULT_GROUP.public.nacos.local.").unwrap());
    c.bench_function("service_key_dotted_service", |b| {
        b.iter(|| black_box(service_key_from_name(&name, &zone)));
    });
}
```

and register them in `criterion_group!` in place of `bench_parse_dns_name_hit, bench_parse_dns_name_miss`:

```rust
criterion_group!(
    benches,
    bench_nacos_authority_lookup_hit,
    bench_nacos_authority_lookup_miss,
    bench_service_key_from_name_hit,
    bench_service_key_from_name_miss,
    bench_service_key_dotted_service,
    bench_to_dns_records_10_ips,
);
```

`LowerName` needs adding to that file's `hickory_proto::rr::{...}` import.

- [ ] **Step 7: Verify, bench, commit**

```bash
cargo bench --bench nacos_bench -- service_key --sample-size 20 --measurement-time 2 --warm-up-time 1
cargo bench -p doggy-dns-nacos --bench nacos_bench -- nacos_authority_lookup --baseline before
cargo bench -p doggy-dns-core --bench handler_bench -- --baseline before
```

Expected: `service_key_from_name_hit` well under the old `parse_dns_name_hit` 0.556 us, and `nacos_authority_lookup_hit` below 1.42 us. Both numbers go in the PR description alongside the spec's targets.

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates/doggy-dns-nacos
git commit -m "$(cat <<'EOF'
perf(nacos): resolve names on labels instead of strings

is_in_zone built two lowercased Strings and a format!(".{zone}") per call and
ran twice per lookup, and parse_dns_name then split the whole name into owned
pieces that were lowercased a third time. Name::zone_of does membership
without allocating, and the key is assembled from borrowed label bytes in one
pass. parse_dns_name, DnsNameParts and is_in_zone are gone; membership and
key building are now separate steps with the order the authority needs.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 11: Parse addresses at push time and answer from the cache guard

**Files:**
- Modify: `crates/doggy-dns-nacos/src/watcher.rs:18-34` (`CachedInstance`), `:56-65` (listener filter)
- Modify: `crates/doggy-dns-nacos/src/mapping.rs` (`to_dns_records`)
- Modify: `crates/doggy-dns-nacos/src/authority.rs` (the cache-read tail of `lookup`)
- Test: `crates/doggy-dns-nacos/tests/{watcher_test,mapping_test,authority_test}.rs`, `crates/doggy-dns-core/tests/handler_test.rs:265-282`, the bench seed sites

**Interfaces:**
- Consumes: `nacos_sdk::api::naming::ServiceInstance` (its `ip` is a `String` on the SDK side, unchanged), `IpAddr::from_str`.
- Produces: `pub struct CachedInstance { pub ip: IpAddr, pub port: i32, pub healthy: bool }`, `CachedInstance::from_service_instance(&ServiceInstance) -> Option<Self>` (replaces `impl From<&ServiceInstance>`), and `to_dns_records(name: &Name, instances: &[CachedInstance], ttl: u32, rtype: RecordType) -> Vec<Record>` (replaces the `&[String]` parameter).

- [ ] **Step 1: Write the failing tests**

In `crates/doggy-dns-nacos/tests/watcher_test.rs`, replace `cached_instance_from_service_instance` and add the rejection case:

```rust
#[test]
fn cached_instance_parses_the_address_once() {
    let si = make_service_instance("192.168.1.100", 9090, true, true);
    let cached = CachedInstance::from_service_instance(&si).expect("valid IPv4");
    assert_eq!(cached.ip, IpAddr::from([192, 168, 1, 100]));
    assert_eq!(cached.port, 9090);
    assert!(cached.healthy);
}

#[test]
fn cached_instance_rejects_a_non_address() {
    let si = make_service_instance("not-an-ip", 9090, true, true);
    assert!(
        CachedInstance::from_service_instance(&si).is_none(),
        "an unparsable address must not reach the cache"
    );
}

#[test]
fn listener_drops_only_the_bad_address() {
    let (listener, cache) = make_listener("default_group", "public");

    let mut broken = make_service_instance("garbage", 8080, true, true);
    broken.metadata = HashMap::new();
    fire_event(
        &listener,
        "svc-partial",
        Some(vec![
            make_service_instance("10.0.0.1", 8080, true, true),
            broken,
        ]),
    );

    let key = ServiceKey::new("svc-partial", "default_group", "public");
    let cached = cache.get(&key).expect("one valid instance is enough");
    assert_eq!(cached.value().len(), 1);
    assert_eq!(cached.value()[0].ip, IpAddr::from([10, 0, 0, 1]));
}
```

Add to that file's imports:

```rust
use std::net::IpAddr;
```

In `crates/doggy-dns-nacos/tests/mapping_test.rs`, replace the four `to_dns_records_*` tests with instances instead of IP strings, and delete `to_dns_records_skips_invalid_ips` (its subject moved to `cached_instance_rejects_a_non_address`):

```rust
fn instances(ips: &[&str]) -> Vec<CachedInstance> {
    ips.iter()
        .map(|ip| CachedInstance {
            ip: ip.parse().unwrap(),
            port: 8080,
            healthy: true,
        })
        .collect()
}

#[test]
fn to_dns_records_creates_a_records() {
    let name = Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &instances(&["10.0.1.5", "10.0.1.6"]), 6, RecordType::A);

    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record.record_type(), RecordType::A);
        assert_eq!(record.ttl, 6);
    }
}

#[test]
fn to_dns_records_handles_ipv6() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(&name, &instances(&["::1"]), 6, RecordType::AAAA);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

#[test]
fn to_dns_records_filters_a_records_for_a_query() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "2001:db8::1"]),
        6,
        RecordType::A,
    );

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::A);
}

#[test]
fn to_dns_records_filters_aaaa_records_for_aaaa_query() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "2001:db8::1"]),
        6,
        RecordType::AAAA,
    );

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_type(), RecordType::AAAA);
}

/// Answer order is the cache order; the record move in the next task depends on
/// it, so pin it down before anything can reorder.
#[test]
fn to_dns_records_preserves_instance_order() {
    let name = Name::from_str("svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let records = to_dns_records(
        &name,
        &instances(&["10.0.1.5", "10.0.1.6", "10.0.1.7"]),
        6,
        RecordType::A,
    );

    let addresses: Vec<String> = records
        .iter()
        .map(|record| record.data.to_string())
        .collect();
    assert_eq!(addresses, ["10.0.1.5", "10.0.1.6", "10.0.1.7"]);
}
```

- [ ] **Step 2: Run the tests to make sure they fail**

Run: `cargo test -p doggy-dns-nacos`
Expected: compile errors - `mismatched types: expected `IpAddr`, found `String`` at every existing `CachedInstance` construction, plus `no associated function named from_service_instance`.

- [ ] **Step 3: Change the cache entry**

In `crates/doggy-dns-nacos/src/watcher.rs`, replace `:18-34`:

```rust
/// Simplified service instance for DNS resolution.
#[derive(Debug, Clone)]
pub struct CachedInstance {
    pub ip: IpAddr,
    pub port: i32,
    pub healthy: bool,
}

impl CachedInstance {
    /// Build a cache entry, rejecting an instance whose address is not an IP.
    ///
    /// Parsed once per push instead of once per query. An unparsable address is
    /// dropped here rather than skipped while answering, so the warn log keeps
    /// its level and text but no longer repeats for every query.
    pub fn from_service_instance(instance: &ServiceInstance) -> Option<Self> {
        match IpAddr::from_str(&instance.ip) {
            Ok(ip) => Some(Self {
                ip,
                port: instance.port,
                healthy: instance.healthy,
            }),
            Err(e) => {
                tracing::warn!("skipping invalid IP '{}': {}", instance.ip, e);
                None
            }
        }
    }
}
```

and in `event` (`:56-65`) use `filter_map`:

```rust
            Some(instances) => {
                let cached: Vec<CachedInstance> = instances
                    .iter()
                    .filter(|i| i.healthy && i.enabled)
                    .filter_map(CachedInstance::from_service_instance)
                    .collect();
                // Immutable replacement: insert replaces the entire value
                self.cache.insert(key, cached);
            }
```

Add to that file's imports:

```rust
use std::net::IpAddr;
use std::str::FromStr;
```

- [ ] **Step 4: Answer from borrowed instances, under the read guard**

In `crates/doggy-dns-nacos/src/mapping.rs`, replace `to_dns_records`:

```rust
/// Convert a service's instances to DNS records matching `rtype`.
///
/// Only records of the requested family are produced, so an `A` query never
/// returns `AAAA` records (and vice versa). The addresses are already parsed -
/// `CachedInstance` holds an `IpAddr` - so this no longer runs
/// `IpAddr::from_str` over every instance for every query.
pub fn to_dns_records(
    name: &Name,
    instances: &[CachedInstance],
    ttl: u32,
    rtype: RecordType,
) -> Vec<Record> {
    instances
        .iter()
        .filter_map(|instance| match (instance.ip, rtype) {
            (IpAddr::V4(ipv4), RecordType::A) => {
                Some(Record::from_rdata(name.clone(), ttl, RData::A(A(ipv4))))
            }
            (IpAddr::V6(ipv6), RecordType::AAAA) => Some(Record::from_rdata(
                name.clone(),
                ttl,
                RData::AAAA(AAAA(ipv6)),
            )),
            _ => None,
        })
        .collect()
}
```

Add `use crate::watcher::CachedInstance;` to that file's imports. `LookupRecords::Records { Arc<RecordSet> }` was considered for sharing one `Name` across the K records and rejected: a `RecordSet` sorts and de-duplicates by rdata, which would change answer order and could drop two instances that share an address. `Section(Vec<Record>)` stays.

In `crates/doggy-dns-nacos/src/authority.rs`, replace the cache-read tail of `lookup` (the `let instances = match … `, the `rtype` check, and the three lines Task 10 kept):

```rust
        // Read through the guard instead of cloning the whole Vec: an answer
        // needs only borrowed instances now. Nothing below may await while
        // `entry` is alive, or a slow shard holder could stall a push writing
        // the same shard - clippy's await_holding_lock does not know about
        // DashMap, so the alloc guard and the async tests are the tripwire.
        let Some(entry) = self.cache.get(&key) else {
            return LookupControlFlow::Continue(Err(LookupError::ResponseCode(
                ResponseCode::NXDomain,
            )));
        };

        // The service exists, but we only serve address records.
        if !matches!(rtype, RecordType::A | RecordType::AAAA) {
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        // Answers carry an owned name, which only this path pays for. K records
        // cost K Name clones; that is the floor a Vec<Record> gives us.
        let answer_name = Name::from(name.clone());
        let records = to_dns_records(&answer_name, entry.value(), self.ttl, rtype);

        if records.is_empty() {
            // Service exists, but has no address of the requested family.
            return LookupControlFlow::Continue(Err(LookupError::NameExists));
        }

        LookupControlFlow::Continue(Ok(AuthLookup::answers(
            LookupRecords::Section(records),
            None,
        )))
```

- [ ] **Step 5: Convert every `CachedInstance` construction**

```bash
cargo test --workspace --no-run 2>&1 | grep "mismatched types" -A3 | grep "\-\->" | sort -u
```

At each site, replace `ip: "10.0.0.1".to_string()` with a parsed address. For tests, `.parse().unwrap()` is the readable form (test files carry `#![allow(clippy::disallowed_methods)]`):

```rust
CachedInstance {
    ip: "10.0.0.1".parse().unwrap(),
    port: 8080,
    healthy: true,
},
```

`crates/doggy-dns-core/tests/handler_test.rs:265-282` takes `&[&str]`, so its map becomes:

```rust
        ips.iter()
            .map(|ip| CachedInstance {
                ip: ip.parse().unwrap(),
                port: 8080,
                healthy: true,
            })
            .collect(),
```

and its assertions that read `instance.ip` back must compare against `IpAddr`. `crates/doggy-dns-nacos/benches/nacos_bench.rs:22` builds addresses from a counter; make the type explicit rather than parsing a formatted string:

```rust
        let instances = vec![CachedInstance {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, (i / 256) as u8, (i % 256) as u8)),
            port: 8080,
            healthy: true,
        }];
```

with `use std::net::{IpAddr, Ipv4Addr};` added. Its `bench_to_dns_records_10_ips` must build the ten instances **outside** the `iter` closure, otherwise the bench measures `Vec` construction rather than record building:

```rust
fn bench_to_dns_records_10_ips(c: &mut Criterion) {
    let name = Name::from_str("my-svc.nacos.local.").unwrap();
    let instances: Vec<CachedInstance> = (0..10)
        .map(|i| CachedInstance {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)),
            port: 8080,
            healthy: true,
        })
        .collect();
    c.bench_function("to_dns_records_10_ips", |b| {
        b.iter(|| black_box(to_dns_records(&name, &instances, 60, RecordType::A)));
    });
}
```

- [ ] **Step 6: Verify, bench, commit**

```bash
cargo test -p doggy-dns-nacos
cargo test -p doggy-dns-core --test wire_golden_test
cargo bench -p doggy-dns-nacos --bench nacos_bench -- nacos_authority_lookup --baseline before
cargo bench -p doggy-dns-core --bench handler_bench -- --baseline before
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates
git commit -m "$(cat <<'EOF'
perf(nacos): parse instance addresses once and answer from the cache guard

Every query copied the whole instance Vec (a String per instance), then a
second Vec of those Strings, then parsed each back into an IpAddr. Parsing at
push time removes the field and the per-query parse, and holding the read
guard removes both copies. Unparsable addresses are dropped when the push
arrives instead of when the answer is built.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 12: Move the answer records instead of cloning them

**Files:**
- Modify: `crates/doggy-dns-core/src/handler.rs:9` (imports) and `:168-170` (the `Ok(lookup)` arm)

**Interfaces:**
- Consumes: `AuthLookup::Records { answers: LookupRecords, additionals: Option<LookupRecords> }` (public, `#[non_exhaustive]` so the match needs no wildcard for other variants - `LookupControlFlow` already narrowed it to `AuthLookup`), `LookupRecords::Section(Vec<Record>)`.
- Produces: `fn take_answer_records(lookup: AuthLookup) -> Vec<Record>`, private to `handler.rs`.

This is a behaviour-preserving refactor, so there is no red test to write first: Task 1's golden-wire assertions are its tests, and they were written to fail if the answer set changes shape. Run them before and after.

- [ ] **Step 1: Confirm the guard is green on the current code**

```bash
cargo test -p doggy-dns-core --test wire_golden_test --test handler_test
```

Expected: all pass. If they do not, stop - something earlier in this plan already moved the bytes, and that is the bug to find.

- [ ] **Step 2: Add the move helper**

In `crates/doggy-dns-core/src/handler.rs`, extend the zone-handler import:

```rust
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupError, LookupOptions, LookupRecords,
    MessageResponseBuilder,
};
```

and add next to `min_ttl_from_records`:

```rust
/// Move the answer records out of a zone lookup instead of cloning them.
///
/// The authorities we own build answers as `LookupRecords::Section`, an owned
/// `Vec`, so handing it over is a free transfer of records that used to cost
/// two allocations per answer through `iter().cloned()`. `forward` and `native`
/// return `AuthLookup::Resolved`, whose records live behind an `Arc`, so those
/// keep the clone path. Order is preserved: `Section` iterates forward, the
/// same direction the clone walked, and `AuthLookup::iter` already ignored
/// `additionals`, so the `..` binding drops nothing the response carried.
fn take_answer_records(lookup: AuthLookup) -> Vec<Record> {
    match lookup {
        AuthLookup::Records {
            answers: LookupRecords::Section(records),
            ..
        } => records,
        lookup => lookup.iter().cloned().collect(),
    }
}
```

- [ ] **Step 3: Use it in the answer arm**

Replace `:168-170`:

```rust
                LookupControlFlow::Continue(Ok(lookup)) | LookupControlFlow::Break(Ok(lookup)) => {
                    let records = take_answer_records(lookup);
                    let answer_count = records.len();
```

Everything downstream (`serialize_for_xdp`'s `records.iter()`, `min_ttl_from_records(&records)`, `MessageResponseBuilder::build(metadata, records.iter(), …)`) already reads `&records`, so it is unchanged.

- [ ] **Step 4: Verify and bench**

```bash
cargo test -p doggy-dns-core --test wire_golden_test --test handler_test
cargo test -p doggy-dns-xdp
cargo bench -p doggy-dns-core --bench handler_bench -- handler_pipeline_nacos_hit --baseline before
```

Expected: golden bytes identical (including the `nacos_hit` case, which is the one with records to move); `handler_pipeline_nacos_hit`'s `change:` line is an improvement, and the spec's target for it is `<= 1.7 us`.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates/doggy-dns-core/src/handler.rs
git commit -m "$(cat <<'EOF'
perf(core): move answer records out of the zone lookup

The chain hands back an owned AuthLookup, and the handler walked it with
iter().cloned() - two allocations per answer for records it already owned.
Take the Section's Vec when the lookup is shaped that way and keep the clone
path for Resolved lookups, whose records are behind an Arc.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 13: Allocation guard on `NacosAuthority::lookup`

**Files:**
- Create: `crates/doggy-dns-nacos/tests/alloc_guard_test.rs`
- Modify: `crates/doggy-dns-nacos/Cargo.toml` (nothing to add - `hickory-server`, `hickory-proto`, `dashmap`, `tokio` are already dependencies, and integration tests see both `[dependencies]` and `[dev-dependencies]`)

**Interfaces:**
- Consumes: `NacosAuthority::new`, `ServiceKey::new`, `CachedInstance { ip: IpAddr, .. }`, `ZoneHandler::lookup`.
- Produces: a test file enforced by the existing `cargo test --workspace` job. No CI change, no production API change. Its `#[global_allocator]` applies only to this test binary, which is why it is its own file.

- [ ] **Step 1: Write the guard with a zero budget so it reports the truth**

Create `crates/doggy-dns-nacos/tests/alloc_guard_test.rs`:

```rust
#![allow(clippy::disallowed_methods)]

//! Allocation counts on the Nacos resolve path.
//!
//! Timings on a shared runner are easy to wave away; an allocation count is not.
//! This wraps the system allocator and measures one `lookup` call, which is the
//! unit of work the hot-path refactor owns. The budgets were recorded from the
//! first green run of this file, not guessed: `String`-free name resolution
//! should cost one key allocation, one answer `Name`, and one `Record` `Name`
//! clone per answer.
//!
//! Both measurements live in one test on purpose: libtest runs tests in a file
//! concurrently, and a second thread allocating inside the measured window would
//! make the count meaningless.

use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupOptions, ZoneHandler};
use std::alloc::{GlobalAlloc, Layout, System};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: this forwards every request to `System` unchanged and only counts the
// allocations it passes through. `dealloc` deliberately does not count, so the
// figure is "allocations requested", which is what the budgets describe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn authority() -> NacosAuthority {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey::new("user-service", "default_group", "public"),
        vec![CachedInstance {
            ip: IpAddr::from([10, 0, 1, 5]),
            port: 8080,
            healthy: true,
        }],
    );
    NacosAuthority::new(cache, "nacos.local", 6)
}

fn measure(name: &str, authority: &NacosAuthority, rtype: RecordType) -> usize {
    let lower = LowerName::from(Name::from_str(name).unwrap());
    ALLOCS.store(0, Ordering::Relaxed);
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(authority.lookup(&lower, rtype, None, LookupOptions::default()));
    // The boxed future and its result are dropped after the read, so they are
    // not attributed to the path under test.
    let count = ALLOCS.load(Ordering::Relaxed);
    assert!(
        matches!(result, hickory_server::zone_handler::LookupControlFlow::Continue(_)),
        "{name}: expected a Continue result inside the zone"
    );
    count
}

#[test]
fn hit_and_miss_paths_allocate_within_budget() {
    let authority = authority();

    let hit = measure("hit", &authority, RecordType::A);
    let miss = measure("miss", &authority, RecordType::A);

    // Zero budgets so the first run prints the measured truth.
    const HIT_BUDGET: usize = 0;
    const MISS_BUDGET: usize = 0;
    assert!(
        hit <= HIT_BUDGET,
        "hit path: {hit} allocations (budget {HIT_BUDGET}) - record the measured count"
    );
    assert!(
        miss <= MISS_BUDGET,
        "miss path: {miss} allocations (budget {MISS_BUDGET}) - record the measured count"
    );
}
```

- [ ] **Step 2: Run it to get the real counts**

```bash
cargo test -p doggy-dns-nacos --test alloc_guard_test
```

Expected: FAIL with `hit path: N allocations (budget 0)` and, in the same message, the miss count. Write both numbers down. Expected order of magnitude per the spec: hit about `4 + 2 x answers` (the composite key, the answer `Name` = 2 allocations, one `Vec<Record>`, the boxed future, and a `Name` clone per record) and miss about `4` (key, boxed future, `LookupError`). If the hit count is far above that, the refactor left something behind - most likely a `to_lowercase`, a `format!`, or a `Vec` clone on the answer path - and finding it is the point of this task.

- [ ] **Step 3: Set the budgets from the measurement**

Replace the two constants with the measured values plus two, and state the reasoning in the comment so a later reader knows the budget is a ratchet, not a guess:

```rust
    // Measured 6 (one instance answering) and 3; two slots of slack so adding
    // an answer record or an EDNS option does not fail CI, and enough pressure
    // that the next `to_string` on this path does.
    const HIT_BUDGET: usize = 8;
    const MISS_BUDGET: usize = 5;
```

Those numbers are placeholders for the values from Step 2 until this plan is executed; the spec's targets are hit `<= 8` and miss `<= 3`, and the constants must not be raised above the spec's targets without a note explaining what changed and why.

- [ ] **Step 4: Verify and commit**

```bash
cargo test -p doggy-dns-nacos
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
git add crates/doggy-dns-nacos/tests/alloc_guard_test.rs
git commit -m "$(cat <<'EOF'
test(nacos): guard the allocation count on the resolve path

Timings on shared runners are arguable; allocation counts are not. Measure
one lookup through a counting global allocator and ratchet the budget, so
the next accidental to_lowercase or Vec clone on the Nacos path fails the
test suite instead of the next review.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

### Task 14: Documentation and the closing measurement

**Files:**
- Modify: `AGENTS.md:81-83` (Nacos flow and name format), `AGENTS.md:16-30` (bench commands now that release/bench builds are LTO'd)
- Modify: `docs/development.md:73` (the CI table's "compile check only" row is no longer the whole story), `docs/development.md:16-32`
- Modify: `docs/architecture.md:58` and the Nacos section around it
- Modify: `docs/benchmarks/userspace-baseline.md` (append the post-refactor numbers)

**Interfaces:**
- Consumes: everything above.
- Produces: docs that describe the code that exists, and a baseline file with the round's outcome.

- [ ] **Step 1: Fix the sentences the refactor invalidated**

`AGENTS.md:83` names a function that no longer exists. Replace `:81-83`:

```markdown
`NacosServiceWatcher` → subscribes to Nacos naming service → `InstanceChangeListener`
receives push events → replaces entire `Vec<CachedInstance>` for a `ServiceKey` in
`DashMap` (immutable update) → `NacosAuthority.lookup()` reads through the `DashMap`
read guard and builds records from borrowed instances.

DNS name format: `<service>.<group>.<namespace>.<zone_suffix>.` (zone membership by
`Name::zone_of`, key by `mapping::service_key_from_name`, split from the right so
service names may contain dots). `ServiceKey` is one composite lowercase string
(`service \u{1} group \u{1} namespace`) and `CachedInstance.ip` is an `IpAddr`, parsed
when the push arrives; the authority never lowercases or re-parses per query because
the handler receives a `LowerName`. Only `A`/`AAAA` record types are served, filtered
by query type. The authority owns its whole zone: unknown services return
authoritative `NXDOMAIN` and missing families/types return `NODATA` instead of falling
through to later plugins.
```

`docs/architecture.md:58` keeps its sentence and gains the two facts that make the cache's cost shape obvious:

```markdown
- Maintains an in-memory `DashMap<ServiceKey, Vec<CachedInstance>>` cache; updates
  replace the entire `Vec` for a key (immutable update). `ServiceKey` is a single
  composite lowercase string and `CachedInstance.ip` is an `IpAddr`, so a query builds
  one key and does no address parsing.
- The apex `SOA` is built once at authority construction and cached again on the
  authority chain entry, so a negative answer costs no extra lookup after the first.
```

- [ ] **Step 2: Document what CI now measures**

In `docs/development.md`, replace the table row at `:73`:

```markdown
| **Benchmarks** | `cargo bench --no-run` (compile check) + `Performance` job (component benches base-vs-PR, one end-to-end dnsperf run) |
```

and add after the bench command block, so a reader knows which number to trust:

```markdown
> The `Performance` job runs the component benches twice in one job (main first,
> PR second) and prints criterion `change:` lines, plus one end-to-end dnsperf run
> against `config/doggy-dns-stress.toml` with `RUST_LOG=warn`. It never fails a PR:
> shared-runner variance is not characterised well enough to gate merges. The
> end-to-end Nacos scenario is absent by necessity — the server aborts boot when
> Nacos is unreachable — so the Nacos paths are covered by benches only.
> `docs/benchmarks/userspace-baseline.md` holds the reference numbers.
```

- [ ] **Step 3: Record the outcome in the baseline document**

Append to `docs/benchmarks/userspace-baseline.md` a section with the actual post-merge numbers the `Performance` job reported on the merged `main` run:

```markdown
## After the hot-path round (recorded <date>, from the merged main run)

| Measurement | Before | After | Target |
|-------------|--------|-------|--------|
| `handler_pipeline_nacos_hit` | 2.34 us | <MEASURED> | <= 1.7 us |
| `nacos_authority_lookup_hit` | 1.42 us | <MEASURED> | <= 0.8 us |
| `service_key_from_name_hit` (was `parse_dns_name_hit` 0.556 us) | 0.556 us | <MEASURED> | <= 0.15 us |
| `handler_pipeline_nacos_nxdomain` | <TASK 3 BASELINE> | <MEASURED> | lower |
| end-to-end dnsperf QPS | <TASK 4 BASELINE> | <MEASURED> | not gated |
| `NacosAuthority::lookup` allocations (hit / miss) | ~20 / ~15 | <MEASURED> | <= 8 / <= 3 |

The allocation row comes from `cargo test -p doggy-dns-nacos --test alloc_guard_test`,
not from a profiler, because it is the number that cannot be argued with.
```

Fill every `<MEASURED>` from a real run; a row left as `<>` is a row not verified.

- [ ] **Step 4: Run the full gate one last time, on the merged branch**

```bash
git status --short
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --workspace
cargo bench --no-run --workspace
cargo test -p doggy-dns-nacos --test alloc_guard_test
cargo test -p doggy-dns-core --test wire_golden_test
```

Expected: a clean `git status`, no fmt/clippy output, every test suite `ok`, every bench target compiled, the alloc guard within budget, the golden bytes unmoved. Then re-read the spec's Goals one by one and say for each whether this round met it, in the closing PR description - including the two it did not: the Nacos end-to-end scenario (impossible without a live server, substituted per the Deviation section) and `<= 0.8 us` on `nacos_authority_lookup_hit` if the measurement says otherwise.

- [ ] **Step 5: Commit**

```bash
git add AGENTS.md docs/architecture.md docs/development.md docs/benchmarks/userspace-baseline.md
git commit -m "$(cat <<'EOF'
docs(perf): describe the resolved path as it now is and record the round

parse_dns_name and is_in_zone no longer exist, the cache key is a composite
string, and CI measures the userspace path instead of only compiling benches.
Also record where each hot-path number landed against the spec's targets,
including the Nacos end-to-end scenario the design assumed was reachable and
is not.

Co-authored-by: Qoder <noreply@qoder.com>
EOF
)"
```

---

## Acceptance checklist

The round is done when every item below has evidence, not assertion:

1. `wire_golden_test`'s four byte constants are unchanged from Task 1 across Tasks 5-13. That is the proof the refactor was representation-only; if one moved, find out why before merging.
2. `alloc_guard_test` is inside budget and the budget is at or below the spec's `hit <= 8 / miss <= 3`, or the file explains why not.
3. The `Performance` job runs on a PR, reports one E2E QPS figure and criterion `change:` lines for `handler_bench`, `chain_bench` and `nacos_bench`, and does not fail the PR.
4. Each merged PR's own `change:` lines are in its description: Task 5's shift, Tasks 6-7's negative-path change, Tasks 8-13's hit-path change against the LTO'd baseline, Task 12's answer-move change.
5. `docs/benchmarks/userspace-baseline.md` has real numbers in every row and names the config, the `RUST_LOG` setting, and the runner.
6. `cargo test --workspace`, `cargo clippy --all-features --all-targets -- -D warnings` and `cargo fmt --all -- --check` are green on the final merged `main`, re-run rather than inherited from a PR job.

## What this plan deliberately does not do

Per the spec's Non-Goals and the user's scope decisions: no `SO_REUSEPORT` fan-out (the one change plausibly worth an order of magnitude, deferred until this round's baseline exists to judge it against), no replacement of hickory's UDP receive path, no XDP cache-mutex or double-serialisation work, no change to any log level or log event, no new backend, and no userspace response cache.
