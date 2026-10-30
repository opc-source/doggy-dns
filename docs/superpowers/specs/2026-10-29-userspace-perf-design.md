# Userspace Hot-Path Performance Design

**Status:** approved (2026-10-29)
**Scope:** userspace request path only. No new authority backends, no XDP kernel-path
changes, no log-level or log-event semantics changes.

## Overview

Doggy-DNS answers a cached-in-Nacos query in **2.34 µs** of handler compute, yet the
end-to-end stress test in CI serves **43,288 QPS** with four worker threads. That gap
says the userspace ceiling is mostly *outside* the handler. This design therefore does
two things at once: it makes the ceiling measurable, and it removes the compute that
measurement can attribute to us.

Measured baseline (local, `cargo bench` on macOS, `bench` profile inheriting the
current release defaults, 20 samples / 2 s measurement; worker_threads 1-4 made no
difference, so none of these numbers are concurrency-bound):

| Measurement | Cost |
|-------------|------|
| `handler_pipeline_nacos_hit` (full pipeline, NoOp response handler) | 2.34 µs |
| `handler_pipeline_nxdomain` | 0.55 µs |
| `chain_bench` nacos hit / empty chain | 1.46 µs / 47 ns |
| `nacos_authority_lookup_hit` / `_miss` | 1.42 µs / 1.20 µs |
| `parse_dns_name_hit` / `_miss` | 0.556 µs / 0.264 µs |
| `to_dns_records_10_ips` | 0.40 µs |
| CI dnsperf, userspace-only (`-T 4`, forward + resolver cache) | 43,288 QPS |
| CI dnsperf, XDP fast path active | 136,103 QPS |

Two facts follow. `parse_dns_name` alone is 39% of `NacosAuthority::lookup` and 24% of
the whole pipeline — the name-parsing path is the largest single slice of compute we own.
And 2.34 µs/query implies roughly 430k QPS per core, while CI observes 43k across four
cores: the rest of the budget goes to transport and process structure, which we can
only act on once it is measurable.

## Goals

- Put a committed, PR-comparable end-to-end throughput number in CI, for the pure
  userspace path. Today `rust.yml`'s bench job only runs `cargo bench --no-run`; nothing
  measures anything on the main path.
- Cut `NacosAuthority::lookup` on the hit path from 1.42 µs to ≤ 0.8 µs by removing the
  string/allocation work and the `Name`/`LowerName` round-trips.
- Make release and bench builds use their full optimisation budget (`lto`,
  `codegen-units`), and stop rebuilding the zone apex `SOA` on every negative answer.
- Keep every public behaviour observable-identical: same response codes, same AA/RA
  flags, same log events and levels, same `[[plugins]]` semantics.

## Non-Goals

Deliberately out of this round, each for a stated reason:

- **`SO_REUSEPORT` socket fan-out.** hickory's `handle_udp` runs one task per socket that
  polls both the receive stream and the outbound queue
  (`hickory-net-0.26.3/src/udp/udp_stream.rs:155-188`), so a single socket serialises
  receive *and* send on one core regardless of `worker_threads`. This is the only change
  plausibly worth an order of magnitude, and it was deferred pending the end-to-end
  baseline from Workstream 1.
- **Replacing hickory's UDP path** (own receive loop, no `SerialMessage` copy, no
  `BufDnsStreamHandle` channel hop per response, `recvmmsg`). Subsystem restructure,
  not a hot-path tweak.
- **XDP cache write behind a global `tokio::Mutex`** and the double serialisation when
  `xdp_enabled`. Kernel fast-path work.
- **Lowering per-request `tracing::info!` volume.** Request logs stay at `info` by
  decision.
- **A userspace DNS response cache.** `forward`/`native` already reuse hickory-resolver's
  cache; `nacos` reads the push-fed cache it already owns.
- **A new backend.** Out of scope by request.

## Falsified assumptions

Recorded so nobody re-litigates them:

- `Request::request_info()` **does not deserialise**. `Request::from_bytes` parses the
  `MessageRequest` once and stores it; `request_info()` only borrows it and calls
  `try_as_query()` (`hickory-server-0.26.3/src/server/request_handler.rs:34-49,78-85`).
  The originally proposed `RequestContext` refactor of the `Middleware` trait — passing a
  pre-parsed request down so five crates stop "re-parsing" — has no cost to save and is
  dropped.
- `panic = "abort"` in `[profile.release]` is rejected: `cargo test --release` and the
  criterion benches need unwinding to isolate a failing case.
- The 64 KiB-stack-buffer fear was unfounded: `MAX_RECEIVE_BUFFER_SIZE` is 4096
  (`hickory-net-0.26.3/src/udp/mod.rs:30`). Per datagram we still pay that zeroing plus
  one `Vec` copy (`udp_stream.rs:185-188`) and one task spawn per request
  (`hickory-server-0.26.3/src/server/mod.rs:484`).

## Workstream 1 — End-to-end performance CI

**Where it goes.** A new `Performance` job in `.github/workflows/rust.yml`, mirroring the
comparison pattern already proven in `xdp-ci.yml` (base first with `--save-baseline`, PR
second with `--baseline base`, both tee'd to `/tmp`, written to `$GITHUB_STEP_SUMMARY` and
posted as a same-repo PR comment). Reusing that pattern keeps one mental model and inherits
its documented cache-staleness fix (`sudo rm -rf target/criterion` before the base run).

**Two scenarios, one binary, two configs.** `config/doggy-dns-stress.toml` already exists
and uses `forward` with `upstream = ["8.8.8.8:53"]`; after the first pass the 1000 names
live in hickory-resolver's cache, so it measures the resolver-cache hit path. A second
config, `config/doggy-dns-stress-nacos.toml`, selects the `nacos` plugin on zone
`doggy.test` with no Nacos server reachable: every in-zone name then takes the
zone-owning `NXDOMAIN` branch (parse → cache miss → `apex_soa` → negative answer), which is
exactly the path Workstreams 2 and 3 change. No cache seeding is needed, and seeding would
require a live Nacos or a new injection endpoint; both are out of scope. The Nacos *hit*
path stays covered by `nacos_bench` and `handler_bench` only — stated as a known gap.

**Query files.** `/tmp/queries.txt` (1000 × `svc-N.nacos.default.doggy.test A`) is already
used. Reuse the same file for both configs so the two scenarios share one workload shape.

**Logging during measurement.** Run with `RUST_LOG=warn`. With the default `info` filter the
handler's per-query `tracing::info!` writes to stderr, which makes the job measure the
runner's log pipe rather than the server. The suppression is documented in the job summary
header so the number is never read as "the default configuration".

**dnsperf invocation.** Match the XDP job: one uncounted warm-up pass
(`-n 1 -l 5`) then `-n 10 -T 4 -l 30`. Keep `-T 4` so the client is not the bottleneck on a
2-vCPU runner, and pin `worker_threads = 4` in both configs.

**Report.** `scripts/xdp_stress_report.py` grows a `--single-mode` rendering (QPS,
avg/min/max latency, completed, lost) alongside the existing userspace/XDP pair, and the
new job renames nothing — the script keeps its name and its current callers, because the
parser is already generic over dnsperf output and a rename would churn the XDP workflow for
no gain. Append the criterion `change:` lines for `-p doggy-dns-core --bench handler_bench`,
`-p doggy-dns-plugin --bench chain_bench`, `-p doggy-dns-nacos --bench nacos_bench`.
Report-only: the job never fails on a regression, because runner variance is not
understood well enough yet to gate merges on it.

**Committed baseline.** `docs/benchmarks/userspace-baseline.md` records the first post-merge
numbers with the exact commands, the runner image, and the date. Later PRs cite deltas
against it in their description.

## Workstream 2 — Nacos name resolution without string churn

**Kill the `Name`/`LowerName` ping-pong.** `LowerName::new(&name)` is `name.to_lowercase()`,
which unconditionally allocates a fresh `label_data` `Vec` and clones `label_ends`
(`hickory-proto-0.26.3/src/rr/domain/name.rs:318-329`) — twice per request today, because
`handler.rs:143` does `request_info.query.name().to_owned()` and `handler.rs:155` converts
it back with `.into()`, and `authority_chain.rs:48` lowercases once more. `RequestInfo`
already carries a `&LowerQuery` whose name is a `&LowerName`, so the chain takes it as is:

- `AuthorityChain::resolve(&self, name: &LowerName, record_type, request_info)`. The handler
  passes `request_info.query.name()` with no owned conversion.
- `ZoneHandler::lookup` already receives `&LowerName`, so `NacosAuthority::lookup` stops
  opening with `Name::from(name.clone())` (`authority.rs:78`) and works on the borrowed
  name until it actually needs an owned `Name` for answer records.

Call-site churn is contained: one production caller (`handler.rs:155`) plus 12 test/bench
callers that wrap their `Name` in `LowerName::from(...)`; `AuthorityChain::new`'s 38 call
sites are untouched.

**Zone membership on labels, not strings.** `is_in_zone` (`mapping.rs:17-22`) builds
`name.to_ascii()`, lowercases it, lowercases the zone again, and allocates
`format!(".{zone}")` for the suffix test — and `parse_dns_name` calls it, while
`authority.rs:81` calls it too, so the whole thing runs twice per lookup. Replace both with
one label-aligned comparison against a `zone_labels: Vec<Box<[u8]>>` precomputed in
`NacosAuthority::new`, compared from the right on already-lowercase label bytes. The
label-aligned requirement (`evilnacos.local` is *not* inside `nacos.local`) keeps its
existing tests as the contract.

**One composite cache key.** `DnsNameParts` currently materialises three `String`s
(`mapping.rs:46-51`) and `authority.rs:110-113` lowercases each again — provable waste,
since the incoming `&LowerName` is already lowercase. Change the cache key to a single
composite `String` (`service.group.namespace`, dotted, from the labels right before the
zone), so a lookup builds one key with one allocation and one hash, and the watcher builds
the same shape once per push. Service names containing dots stay supported because the
split is from the right and the separator count is positional, exactly as today.

**Stop deep-copying the instance list.** `authority.rs:115-116` does `entry.value().clone()`,
copying the whole `Vec<CachedInstance>` including one `String` per instance, then
`authority.rs:129` copies every IP into a second `Vec<String>` before
`to_dns_records` parses each one back into `IpAddr`. Instead hold the `DashMap` read guard
and build `Vec<Record>` from `&[CachedInstance]` directly. The guard is held only across
synchronous record building — no `.await` inside the critical section, which is the
condition for that to be safe, and the spec calls it out because it is the one way this
change could deadlock if done carelessly.

**Store parsed IPs.** `CachedInstance.ip` is `String` (`watcher.rs:21`), so every query
re-parses every instance's address and compares it to the query family. Making it
`IpAddr` removes the per-query parse and the per-push `String` clone. `healthy` filtering
is unchanged (still not filtered — a separate correctness question, not a performance one).

**Record construction stays owned.** `AuthLookup::Section(Vec<Record>)` requires an owned
`Name` per record, so K answer records cost K `Name` clones. That is the floor hickory
gives us; the win is reaching it only on the answer path, after `Skip` and `NODATA` have
already returned.

**Allocation guard.** Add `crates/doggy-dns-nacos/tests/alloc_guard_test.rs`: a counting
`#[global_allocator]` wrapping `System`, one `lookup()` on a seeded hit and one on an
in-zone miss, asserting allocation counts below thresholds recorded from the first green
run (expected order of magnitude: hit ≤ 8, miss ≤ 3 today versus ~20/~15 now). The test
lives in its own file so its allocator does not observe other tests, runs on the current
single-threaded async runtime, and is enforced by the existing `cargo test --workspace`
job — no CI change needed. Thresholds are set from measurement, not guessed, in the
implementation plan.

**Targets.** `nacos_authority_lookup_hit` 1.42 µs → ≤ 0.8 µs, `parse_dns_name_hit` 0.556 µs
→ ≤ 0.15 µs, `handler_pipeline_nacos_hit` 2.34 µs → ≤ 1.7 µs. Verified by the Workstream 1
bench comparison.

## Workstream 3 — Build budget and negative-answer cost

**`[profile.release]` in the workspace root** (`Cargo.toml` currently has no profile
section, so release builds get `lto = false`, `codegen-units = 16`):

```toml
[profile.release]
lto = "fat"
codegen-units = 1
strip = "symbols"
```

`panic` stays `unwind` (see falsified assumptions). `bench` inherits `release`, so this
shifts every criterion number on its own; it lands as its own PR immediately after
Workstream 1 so the later Nacos delta is not credited to LTO.

**Cache the zone apex `SOA`.** Every in-zone negative answer goes through
`authority_chain.rs:69` → `apex_soa()` → a *second* `lookup()` on the same handler, and
`NacosAuthority::soa_record()` (`authority.rs:42-48`) rebuilds it each time: two `format!`,
two `Name::from_ascii`, two extra `Name` clones. Since the answer sits in the
`handler_pipeline_nxdomain` path that the Nacos stress scenario hammers, do both halves:

- `NacosAuthority::new` builds the apex `SOA` once into an `apex_soa: Record` field;
  `soa_record()` becomes a clone.
- `NamedHandler` gains `apex_soa: OnceLock<Option<Record>>`, filled on the first
  authoritative negative answer and cloned thereafter, so later negatives cost one `Record`
  clone and no async lookup. `Record: Send + Sync` holds — hickory already shares it across
  tasks inside `Arc<RecordSet>`.

Eagerly fetching at construction was rejected: it means making `AuthorityChain::new`
async, which touches all 38 call sites for a benefit confined to the first query.
Non-authoritative handlers keep `soa: None`, unchanged.

## Workstream 4 — Move the answer records instead of cloning them

`handler.rs:169` does `let records: Vec<_> = lookup.iter().cloned().collect()`. `lookup` is
already an owned `AuthLookup` handed back by the chain, and our zone authorities return
`AuthLookup::Records { answers: LookupRecords::Section(Vec<Record>), additionals: None }` —
a public variant holding an owned `Vec`. Matching that shape and moving the `Vec` out turns
K `Record` clones (each cloning a `Name`, so 2+ allocations per record) into a free
transfer. `forward` and `native` return `AuthLookup::Resolved(lookup)`
(`builtin/forward.rs:153`, `builtin/native.rs:63`), where the records live behind the
resolver's `Arc`, so those keep the existing `iter().cloned()` path as the fallback. Small,
self-contained, and it sits directly on the Nacos hit path that `handler_bench` measures.

## Rollout order

1. **W1 — CI performance job + baseline doc.** Nothing else can be claimed without it.
2. **W3a — `[profile.release]`.** Ten minutes, shifts the whole curve, must be isolated.
3. **W3b — apex `SOA` precompute + chain cache.** Small, lands on the W3 branch.
4. **W2 — Nacos resolution rewrite.** Largest surface, best-documented targets, split
   into (a) `resolve(&LowerName)` plumbing, (b) zone membership + composite key, (c) guard
   and parsed IPs, as three reviewable commits on one PR if the diff grows.
5. **W4 — record move.** Rides along at the end, where its effect is measurable against a
   settled baseline rather than tangled into W2's diff.

Each step is one reviewable PR with its own base-vs-PR numbers in the description.

## Testing and verification

- Every step: `cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D
  warnings`, `cargo test --workspace`.
- W2 additionally: `cargo test -p doggy-dns-nacos` (zone-boundary, `NODATA`, family
  filtering, dotted-service names), `cargo test -p doggy-dns-plugin --test
  authority_chain_test`, `cargo test -p doggy-dns-core --test handler_test`, the new alloc
  guard, and `cargo bench --bench nacos_bench` / `--bench chain_bench` /
  `-p doggy-dns-core --bench handler_bench`.
- W2 case-comparison check: a query whose labels are uppercase on the wire (`dig` sends
  them as typed) must still resolve — the lowercase guarantee moves from
  `to_lowercase()` calls to the `&LowerName` contract, so a test asserting an
  `SERVICE.GROUP.NS.zone` query hits the same cache entry is required, not optional.
- W3b check: `NXDOMAIN` responses must keep carrying the apex `SOA` in the authority
  section with the same TTL (`authority_test.rs` already asserts the record; extend it to
  assert two consecutive negatives return equal SOA rdata, proving the cache is filled).
- W2/W4 output check: the changes are representation-only, so add one golden-wire test
  asserting that a fixed query set (Nacos hit, `NODATA`, in-zone `NXDOMAIN`, apex `SOA`)
  serialises to the same bytes as today, captured before the refactor. If the bytes move,
  the refactor is wrong, not the expectation.
- W1 check: run the job on a throwaway PR against `main` and confirm base and PR numbers
  agree within runner noise before trusting any later delta.
- Post-merge: record the new numbers in `docs/benchmarks/userspace-baseline.md`.

## Risks

- **Composite-key collision.** `service.group.namespace` is ambiguous if a service name can
  itself contain the separator pattern that a group/namespace would produce. Mitigation:
  the split is positional from the right and the watcher is the only other key producer, so
  both sides use the same rule; a unit test asserts that a service literally named
  `a.b` in group `c`, namespace `d` is distinct from service `a`, group `b.c`, namespace
  `d`. If the assertion cannot hold, keep `ServiceKey` as three fields and accept the three
  key allocations instead — decided in the plan, not silently.
- **DashMap guard across record building.** Safe only while no `.await` happens under the
  guard; the alloc guard and the existing async tests are the tripwire.
- **`OnceLock` holds a stale SOA if TTL config were ever hot-reloaded.** Today TTL is fixed
  at startup; if remote config ever rewrites plugins, this cache must be invalidated with
  the chain. Noted in the code comment so the constraint is visible.
- **W4 record ordering.** Answer order is currently whatever the authority built, and the
  move must not reorder records relative to the `min_ttl` computation at `handler.rs:192` or
  the `additionals` slot; the match is restricted to `additionals: None` so a zone answer
  carrying extras keeps the clone path instead of silently dropping them.
- **CI variance.** 2-vCPU shared runners make absolute QPS meaningless; only same-job
  base-vs-PR comparisons are reported, and the job stays report-only.
- **`lto = "fat"` build-time cost** on the `Build & Test` and `Performance` jobs; accepted
  for now, revisited if CI wall time becomes the complaint.
