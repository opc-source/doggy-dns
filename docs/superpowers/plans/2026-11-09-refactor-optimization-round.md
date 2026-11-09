# Refactor & Optimization Round (2026-11-09) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the 12 confirmed findings from the 2026-11-09 review as 9 independent PRs (A–I), each reviewable on its own.

**Architecture:** No behavior changes except two XDP correctness fixes (PR A). Everything else is allocation removal, deduplication, or build/CI plumbing pinned by existing or new tests. PR C is measurement-gated: keep only if it does not regress.

**Tech Stack:** Rust edition 2024, hickory 0.26, aya 0.16 (git tag), nacos-sdk 0.8, opentelemetry 0.33, rmcp 3.7, criterion 0.8.

**Spec:** the 2026-11-09 conversation review (12 numbered findings); this plan is the executable form.

## Global Constraints

- Baseline `e2196c3` (main == origin/main): fmt/clippy/test all green on host Rust 1.99.1. Re-run gates yourself; never trust piped `$?`.
- No `.unwrap()` in lib/bench code (tests: `#![allow(clippy::disallowed_methods)]` at file root). No `std::time::Instant`, no `std::thread::sleep`, no `std::env::set_var` in lib code.
- `cargo clippy --all-features --all-targets -- -D warnings` and `cargo fmt --all -- --check` must pass; eBPF workspace fmt runs on the macOS host (`cd xdp && cargo fmt --all -- --check`), builds/tests in the container.
- Watcher semantics (ledger/backoff/entry_of), rotation cursor, authority-chain order, XDP cache-freeze replay: unchanged.
- Worktrees: `.worktrees/<branch>` (already in `.git/info/exclude`). Host gates use `CARGO_TARGET_DIR=/Users/cherishcai/opensource/doggy-dns/target` (sequential dispatch only).
- Linux/XDP verification: `localhost/xdp-gcc:snap` privileged container; copy repo in with `git archive <commit> | podman exec -i <c> tar -x -C /tmp/repo` (committed snapshot only), `mount -t bpf bpf /sys/fs/bpf`, `rm -rf /sys/fs/bpf/doggy-dns-test` before integration tests; build `-j 2`; eBPF: `cargo build -Z build-std=core,alloc --release --target bpfel-unknown-none` inside `xdp/doggy-dns-xdp-ebpf`.
- Commits: conventional style matching `git log`; add `Co-authored-by: Qoder <noreply@qoder.com>` trailer. Push + PR only after review and green gates. Never merge (user's call). Never push main.
- PR C branches from PR B's branch (stacked; retarget to main after B merges). PR G runs last (touches test files everywhere).

## PR order

A → B → C(stacked on B) → D → E → F → H → I → G

---

### Task 1 (PR A): XDP kernel correctness — 0x20 question replay + fragment pass

**Branch:** `fix/xdp-question-case-fragments`

**Files:**
- Modify: `xdp/doggy-dns-xdp-ebpf/src/main.rs` (fragment check after :126; `write_positive` :463-520)
- Test: `crates/doggy-dns-xdp/tests/xdp_integration_test.rs` (follow existing attach-lock + raw-UDP patterns; lo attach is single — reuse the in-process lock, loser skips)

**Interfaces:**
- Consumes: `read_u16_be`, `write_u16_be`, `copy_chunk` from `protocol.rs`; `CacheValue` from abi (unchanged).
- Produces: no API change. `write_positive` keeps signature `(ctx, dns, end, entry, resp_len, txid)`; `txid` param becomes unused → remove it from signature and call site (:261) OR keep and drop the overwrite; choose removal, update the call.

**Design (verified against code):**
- Cache key hashes lowercased wire bytes on both sides (`cache.rs:63`, `main.rs:347/382`), so a hit matches any case. `write_positive` today copies the whole cached reply and rewrites only txid (:517) — the question section carries the cache-writer's case. 0x20-validating clients drop that reply.
- Hash equality implies identical QNAME wire length, so the cached response's question section has exactly `question_len` bytes, same as the incoming query. `bpf_xdp_adjust_tail` preserves the front of the packet: the incoming question is still in the frame after resize.
- New `write_positive` (pass `question_len: usize` in place of `txid`):
  1. Guard `dns + DNS_HDR_LEN > end` → error+Err (resp_len ≥ MIN_POSITIVE_LEN=17 makes this a verifier formality).
  2. Copy cached header words at offsets 2,4,6,8,10 with `write_u16_be(dns as *mut u8, off, read_u16_be(src, off))`. Offset 0 (txid) is NOT written — the incoming txid is already there.
  3. Leave `[DNS_HDR_LEN, DNS_HDR_LEN + question_len)` untouched — that is the incoming question, case intact.
  4. Copy cached tail `[s, resp_len)` where `s = DNS_HDR_LEN + question_len`, from `entry.resp_data` at the same offsets. `tail_len = resp_len - s` (resp_len ≥ s always: a NODATA positive has resp_len == s → skip). If `tail_len == 0`: done. If `tail_len < COPY_CHUNK`: byte loop `for k in 0..tail_len` with `dns + s + k + 1 > end` guard (bounded by constant 16). Else reuse the existing lead/chunk pattern shifted by `s`: `lead = tail_len & (COPY_CHUNK - 1)`; if lead != 0 guard `dns + s + COPY_CHUNK > end` then `copy_chunk(dns + s, src, s)`; loop `i = s + lead; while i < resp_len { guard dns + i + COPY_CHUNK > end; copy_chunk(dns + i, src, i); i += COPY_CHUNK }`.
  5. Verifier fallbacks, in order, if the shifted chunk loop is rejected: (a) byte-wise copy of the whole tail (bounded by MAX_RESPONSE_SIZE=512); (b) save the incoming question to a `[u8; 259]` stack buffer before the copy and restore after (watch the 512-byte BPF stack — check with `llvm-objdump`/verifier log). Do not give up until one shape passes; record which one in the PR body.
- Fragment check in `try_dns_xdp`, right after the `ip_hdr_len` validation (:123-126):
  ```rust
  // A fragmented datagram is not a DNS query we can answer: non-first
  // fragments carry no UDP header, and a first fragment must not be
  // answered with the MF/offset bits still set. Pass the whole thing.
  let frag_off = read_u16_be((data + ip_offset) as *const u8, 6);
  if frag_off & 0x3FFF != 0 {
      return Ok((xdp_action::XDP_PASS, None));
  }
  ```
  (`0x3FFF` = MF bit + fragment offset; `finalize_headers` :573 keeps setting DF on replies, unchanged.)

- [ ] **Step 1: Write failing integration tests** (container-run, `#[ignore]`):
  - `positive_hit_preserves_query_case`: populate the cache through the real middleware path with a response whose question is `TeSt.ExAmPlE.` (build the response bytes via hickory `Message` with that exact case; insert with `compute_cache_entry` + `DnsCacheMap`), then send a raw UDP query for `tEsT.eXaMpLe.` (same wire length). Assert: reply question bytes equal the QUERY's case byte-for-byte, txid equals the query's, answer records match the cached ones, stats `hit` incremented.
  - `fragmented_query_passes`: hand-build an Ethernet+IPv4+UDP frame with `frag_off = 0x2000` (MF, offset 0) and a valid cacheable query payload; send via the existing raw-frame harness (AF_PACKET or the test's existing injector — follow `xdp_integration_test.rs` patterns). Assert: no XDP_TX reply, stats unchanged (no hit/miss/drop), i.e. PASS. Repeat with `frag_off = 0x0001` (non-first fragment).
  - Red check: run both against the pre-change ELF in the container → case test FAILS (question comes back `TeSt.ExAmPlE.`), fragment test FAILS or is unsafe; capture output for the PR body.
- [ ] **Step 2: Implement** the two changes above; build eBPF in the container until the verifier accepts; run `readelf -S` sanity + the full `xdp_integration_test` suite (`--ignored --nocapture`, bpffs mounted, pins cleared).
- [ ] **Step 3: Host gates** — `cd xdp && cargo fmt --all -- --check`; `cargo fmt --all -- --check`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo test --workspace` (host, xdp integration stays ignored).
- [ ] **Step 4: Commit** `fix(xdp): replay the query's own question case and pass fragments` with the co-author trailer. PR body: mechanism, verifier shape chosen, red/green test output, container kernel version.

---

### Task 2 (PR B): hot-path residual allocations

**Branch:** `perf/hot-path-residual-allocs`

**Files:**
- Modify: `crates/doggy-dns-observability/src/metrics.rs:151-172` (+ inline tests :174-250)
- Modify: `crates/doggy-dns-plugin/src/builtin/native.rs:53-88`, `builtin/forward.rs:126-159` (and the rest of each lookup's log sites)
- Modify: `crates/doggy-dns-xdp/src/middleware.rs:16-44`, `src/middleware/xdp_cache.rs:68-77`
- Test: existing inline metrics tests; `crates/doggy-dns-xdp/tests/` compute_cache_entry callers; new value-pinning cases below

**Interfaces:**
- `fn outcome_attributes(request: &Request, outcome: &RequestOutcome) -> [KeyValue; 4]` (was `Vec<KeyValue>`); `record()` at :61-62 passes `&attributes`.
- `compute_cache_entry(qname, qtype, qclass, response_code, resp_bytes: &[u8], min_answer_ttl, negative_ttl, now_ns)` — parameter becomes a slice; `xdp_cache.rs` passes `resp_bytes` directly (drop `.clone()`).

**Design:**
- Exact string preservation is the contract. Old outputs: qtype `format!("{:?}", RecordType)`, protocol `format!("{:?}", Protocol).to_ascii_lowercase()`, rcode `format!("{:?}", ResponseCode).to_ascii_uppercase()`, authority `Arc<str>` deref or `"none"`.
- Helpers return `opentelemetry::Value` (avoid Cow/Into ambiguity):
  ```rust
  fn qtype_value(t: RecordType) -> Value {
      match t {
          RecordType::A => Value::from("A"),
          RecordType::AAAA => Value::from("AAAA"),
          // … one arm per hickory RecordType variant, spelling == Debug output …
          other => Value::from(format!("{other:?}")), // Unknown/zero fallback, rare
      }
  }
  ```
  Same shape for `protocol_value` (lowercase spellings: `udp`, `tcp`, `tls`, `https`, … enumerate from hickory `net::xfer::Protocol`) and `rcode_value` (uppercase: `NOERROR`, `FORMERR`, `SERVFAIL`, `NXDOMAIN`, … enumerate `ResponseCode` incl. any numeric fallback variant). Enumerate variants from the vendored hickory-proto 0.26 source (`~/.cargo/registry/src/…`), not from memory; `#[non_exhaustive]` enums need the wildcard arm anyway.
- Authority stays `Value::from(outcome.authority.as_deref().unwrap_or("none").to_string())` — config-derived, not static; one String per query remains and that is fine.
- Array return kills the Vec allocation; static arms kill the three `format!`s on common types.
- native/forward: delete `let name_str = name.to_string();`, replace every `%name_str` with `%name` (LowerName: Display produces the identical string — `to_string()` was Display anyway). In forward's `info_span!`, `"dns.question.name" = %name`.

- [ ] **Step 1: Pin current behavior first** — extend the inline metrics tests: assert all four attribute values for A/AAAA/HTTPS/TCP/TLS/NoError/NXDomain/ServFail AND the fallback spellings for an exotic qtype (e.g. `RecordType::Unknown(65534)` if that variant exists — check vendored source; otherwise `RecordType::OPT`/`ANY`) and an exotic rcode. Run against CURRENT code → green (these are characterization tests).
- [ ] **Step 2: Allocation proof (temporary harness, not committed)** — in a scratch copy, wrap the System allocator with a counter (pattern: `crates/doggy-dns-nacos/tests/alloc_guard_test.rs`), measure one `outcome_attributes` call via a temporary `#[cfg(test)]` test in metrics.rs run SINGLE-THREADED (`--test-threads=1`): record before-count. Implement the change, re-run: after-count must be strictly lower (expect: before ≥ 6, after ≤ 2). Put both numbers in the PR body; delete the scratch harness. (No committed alloc guard here: the function is private and the lib-test binary runs concurrent tests — a committed budget would be noise.)
- [ ] **Step 3: Implement** metrics + native/forward + xdp slice changes; update `compute_cache_entry` test callers to pass `&bytes`.
- [ ] **Step 4: Gates** — characterization tests green; host fmt/clippy/test; container `cargo check -p doggy-dns-xdp` (Linux-only `xdp_cache.rs` call site) and `cargo test -p doggy-dns-xdp --lib` there.
- [ ] **Step 5: Commit** `perf: drop residual per-query allocations in metrics, native/forward and the XDP cache write` + trailer.

---

### Task 3 (PR C, stacked on B): XDP cache-write lock removal — measurement-gated

**Branch:** `perf/xdp-cache-lockfree` FROM `perf/hot-path-residual-allocs`

**Files:**
- Modify: `crates/doggy-dns-xdp/src/cache/bpf_map.rs` (add `try_clone`), `src/middleware/xdp_cache.rs`
- Bench: `crates/doggy-dns-xdp/benches/xdp_component_bench.rs` (read what it covers; add a cache-write group only if missing)

**Design & decision gate:**
- Today: `Arc<Mutex<DnsCacheMap>>` serializes every worker's `insert` (a `bpf_map_update_elem` syscall) behind one tokio mutex. The kernel side is already thread-safe; aya map handles can be duplicated.
- Step 0 (verify before coding): read vendored aya 0.16 (`~/.cargo/git/checkouts/aya-*/…`): does `Map::try_clone(&self) -> Result<Map>` exist? Is `MapData: Sync`? Record findings.
- `DnsCacheMap::try_clone(&self) -> Result<Self>` via `self.inner.map().try_clone()` + `HashMap::try_from`.
- Middleware: if `DnsCacheMap: Sync`, hold the template directly; else keep `Mutex<DnsCacheMap>` touched ONLY at per-thread init. Hot path:
  ```rust
  thread_local! {
      static CACHE_HANDLE: RefCell<Option<DnsCacheMap>> = const { RefCell::new(None) };
  }
  ```
  Init once per tokio worker thread from the template; `insert` without `.await`, without lock. tokio workers are long-lived OS threads → one fd dup per worker, not per query.
- Measurement: criterion before/after on the cache-write path, in the container, both runs with `CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_RELEASE_LTO=false` (same override both sides; VM can't link fat-LTO benches reliably). Also run the multi-thread stress shape if the bench has one.
- **Gate:** keep if no regression (>5% slower = regress) and the lock is gone from the hot path; if numbers are flat-to-worse or aya lacks `try_clone`, DISCARD the branch (precedent: SO_REUSEPORT multi-listener), report numbers, close task. Do not ship a speculative concurrency change without numbers.

- [ ] Steps: verify aya API → add bench group if missing → baseline numbers (B-branch snapshot) → implement → after numbers → decision → (if kept) container full xdp tests + host gates → commit `perf(xdp): duplicate the cache map handle per worker instead of one global mutex` → PR base = B branch, note stacking in body.

---

### Task 4 (PR D): single-source TLS config loading

**Branch:** `refactor/tls-load-single-source`

**Files:**
- Modify: `crates/doggy-dns-core/src/server.rs:62-130`
- Test: `crates/doggy-dns-core/tests/` (new `tls_config_test.rs`; check for existing cert fixtures first — `grep -rn "cert" crates/doggy-dns-core/tests/`)

**Interfaces:**
- `fn load_rustls_config(cert_path: &str, key_path: &str, alpn: Option<Vec<Vec<u8>>>) -> Result<rustls::ServerConfig>` in `server.rs` (private). Reads PEM, parses certs + key, `with_no_client_auth().with_single_cert`, sets ALPN when given. EVERY fallible step carries `.context(...)` — including the `with_single_cert` call that HTTPS currently leaves bare (:114).
- `register_tls` → `load_rustls_config(&tls.cert_path, &tls.key_path, None)`; `register_https` → `… Some(vec![b"h2".to_vec()])`.
- basic's `TlsConfig`/`HttpsConfig` stay SEPARATE types: their serde port defaults (853/443) are real per-type semantics; merging would change config behavior. (Deliberate non-goal.)

- [ ] **Step 1: Failing tests** — `tls_config_test.rs`: missing cert file → error chain contains "failed to read TLS certificate"; garbage PEM → "failed to parse TLS certificate"; cert-only PEM (no key) → "no private key found"; valid pair → Ok, and with ALPN → `alpn_protocols == [b"h2"]`. Generate fixtures with `rcgen` (add as dev-dependency ONLY if no fixture exists; check workspace first) or include static PEM fixtures. Run → red (function doesn't exist).
- [ ] **Step 2: Implement**, wire both callers, delete the duplicated blocks.
- [ ] **Step 3: Gates** fmt/clippy/test workspace. **Step 4: Commit** `refactor(core): single-source TLS/HTTPS certificate loading` + trailer.

---

### Task 5 (PR E): control responses as typed Serialize structs

**Branch:** `refactor/control-typed-json`

**Files:**
- Modify: `crates/doggy-dns-control/src/server.rs` (:46-76 get_status, :97-98 list_plugins wrapper, :121-128 get_config, :154-159 error helper), `src/lib.rs:73-121` (PluginSnapshot three branches), `src/live.rs:44-70`, `src/transport.rs:213-216` (healthz)
- Test: existing `tests/http_contract_test.rs`, `tests/tools_test.rs`, `tests/startup_test.rs` are the contract — they must pass UNCHANGED.

**Design:**
- One `#[derive(serde::Serialize)]` struct per response shape, `#[serde(rename_all = "camelCase")]` where keys are camelCase (match EXACT current key spellings — read each Map site). PluginSnapshot branches → `#[serde(untagged)]` enum or per-kind structs; keep the emitted field sets identical (nacos: name/group/…/ttl; native: cacheSize/minTtl/maxTtl; forward: upstream/cacheSize/minTtl/maxTtl — verify against lib.rs).
- `serde_json` has NO `preserve_order` feature here → current Map output is key-sorted; struct output is declaration-ordered. Contract tests must parse JSON (verify — if any asserts raw string equality, declare fields in the sorted order to keep bytes identical).
- Error helper → `struct ErrorBody { error: ErrorDetail }` etc. `json!` macro stays banned; derive is the sanctioned shape.

- [ ] **Step 1:** run control tests, confirm green baseline; read every Map site and write the struct definitions to match field-for-field.
- [ ] **Step 2:** replace all `Map::new()` sites (grep must return zero in `crates/doggy-dns-control/src/` afterwards).
- [ ] **Step 3:** `cargo test -p doggy-dns-control` UNCHANGED tests green; fmt/clippy/test workspace.
- [ ] **Step 4: Commit** `refactor(control): type the JSON responses instead of hand-building maps` + trailer.

---

### Task 6 (PR F): OTLP exporter setup single-sourced

**Branch:** `refactor/otlp-exporter-setup`

**Files:**
- Create: `crates/doggy-dns-observability/src/exporter.rs`
- Modify: `src/lib.rs:67-93` (init), `src/logs.rs:10-24`, `src/traces.rs:11-25`, `src/metrics.rs:102-117`, `src/profiles/mod.rs:78-85`

**Interfaces:**
- `auth::resolve_headers(config) -> Result<HashMap<String, String>>` (verified). Init calls it ONCE when `config.enabled` and passes clones down:
  - `build_logger_provider(config, resource, headers: HashMap<String,String>)`
  - `build_meter_provider(config, resource, headers) -> Result<(SdkMeterProvider, DnsMetrics, XdpMetrics)>`
  - `build_tracer_provider(config, resource, headers)`
  - `ProfileExporter::new(config, resource, headers)` — keeps its own `to_header_map` + content-type override.
- `exporter.rs`:
  ```rust
  use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
  pub(crate) fn configured_http<B: WithExportConfig + WithHttpConfig>(
      builder: B, endpoint: String, headers: HashMap<String, String>, timeout_secs: u64,
  ) -> B {
      builder.with_protocol(Protocol::HttpBinary)
          .with_endpoint(endpoint).with_headers(headers)
          .with_timeout(Duration::from_secs(timeout_secs))
  }
  ```
  Both traits are already public imports in logs.rs/traces.rs — feasible. If a builder's `with_*` set doesn't line up (compile error), keep the chain inline for that signal and only ship the headers-once dedup; note it in the PR.

- [ ] **Step 1:** characterization — existing observability tests green baseline; add a test that init with invalid auth config surfaces the same error as today (check existing auth tests first; reuse, don't duplicate).
- [ ] **Step 2:** implement; `grep -rn "resolve_headers" src/` shows exactly ONE call site (lib.rs) plus auth.rs definition/tests.
- [ ] **Step 3:** fmt/clippy/test workspace. **Step 4: Commit** `refactor(observability): resolve OTLP headers once and share the HTTP exporter setup` + trailer.

---

### Task 7 (PR H): build.rs OUT_DIR + workspace dependencies

**Branch:** `chore/build-out-dir-workspace-deps`

**Files:**
- Modify: `crates/doggy-dns-xdp/build.rs`, root `Cargo.toml`, `crates/doggy-dns-core/Cargo.toml:18`, `crates/doggy-dns-observability/Cargo.toml:32`, `crates/doggy-dns-xdp/Cargo.toml:21`
- Read first: `.github/workflows/xdp-ci.yml` (CI pre-builds eBPF into `xdp/target`; after this change build.rs rebuilds into OUT_DIR on CI — the nested build must succeed there; keep CI's explicit build step, it feeds other consumers).

**Design:**
- `let out_dir = PathBuf::from(env!("OUT_DIR"));` nested cargo gets `--target-dir <out_dir>/ebpf`; `elf_path = out_dir/ebpf/bpfel-unknown-none/release/doggy-dns-xdp-ebpf`. `needs_rebuild` logic unchanged (OUT_DIR persists across rebuilds for a stable fingerprint).
- Non-Linux TARGET skip: `if env!("CARGO_CFG_TARGET_OS") != "linux"` → write the zero-byte placeholder into OUT_DIR immediately, `cargo:warning=eBPF build skipped (target is not Linux)`, never spawn nested cargo. (Today macOS attempts a doomed nested build every time.)
- `program.rs:48` already reads `env!("XDP_EBPF_ELF")` — no change there.
- Workspace deps: add `libc = "0.2"` and `core_affinity = "0.8"` to `[workspace.dependencies]`; the three crates + root use `{ workspace = true }`.

- [ ] **Step 1:** host `cargo build -p doggy-dns-xdp` → warning says skipped, placeholder under `target/…/out`, NO `xdp/target` pollution (verify path mtime). `cargo test --workspace` green.
- [ ] **Step 2:** container: committed snapshot, `cargo build -p doggy-dns-xdp -j 2` → real ELF embedded (build.rs nested build succeeds), `cargo test -p doggy-dns-xdp --test xdp_integration_test -- --ignored` green with bpffs mounted.
- [ ] **Step 3:** fmt/clippy; commit `chore(build): put the eBPF artifact in OUT_DIR, skip non-Linux targets, workspace-ize libc/core_affinity` + trailer. PR body notes CI must confirm the nested-build path (xdp-ci runs on the PR).

---

### Task 8 (PR I): CI shared composite actions

**Branch:** `ci/shared-actions`

**Files:**
- Create: `.github/actions/setup-bpf/action.yml`, `.github/actions/dnsperf-run/action.yml`, `.github/actions/pr-comment/action.yml`
- Modify: `.github/workflows/xdp-ci.yml` (3× bpf-linker install :46-58/:71-…/:175-…, 2× github-script :149/:230, dnsperf block :176-217), `.github/workflows/rust.yml` (performance job :69-135, github-script :165), `.github/workflows/perf-bundle.yml` (:55-57 install, :90-111 dnsperf)

**Design:**
- `setup-bpf`: inputs `bpf-linker-version` (default `v0.12.0`); steps = read the FULL existing install blocks (they include toolchain setup beyond the curl — replicate exactly, x86_64 musl binary). Replace all 4 sites with `uses: ./.github/actions/setup-bpf`.
- `dnsperf-run`: inputs `port`, `queries-file-content` or `setup-script`, `output`, `sudo` (bool), `count`, `threads`, `duration`; steps = install dnsperf, write query file, warmup (`-n 1 -l 5`), run (`-n 100000 -T 4 -l 30` defaults matching today). Call sites keep their own report scripts (`scripts/xdp_stress_report.py`, e2e report) — do NOT merge the reports.
- `pr-comment`: inputs `marker`, `body-file`; the github-script create-or-update-with-marker logic (read both blocks; they share the pattern). Replace the 3 sites.
- No behavior change: same commands, same order, same env. actionlint locally if available (`which actionlint`; do NOT install without asking) else careful YAML review + the PR's own CI run is the verification.

- [ ] Steps: read all three workflows fully → extract → replace → grep proves zero duplicate curl/dnsperf install lines → commit `ci: extract setup-bpf, dnsperf-run and pr-comment composite actions` + trailer. PR body: CI on this PR is the test; list which jobs exercise which action.

---

### Task 9 (PR G, LAST): test infrastructure dedup

**Branch:** `test/dedup-infrastructure`

**Files:**
- Create: `crates/doggy-dns-testutil/` (`Cargo.toml` with `publish = false`, `src/lib.rs`), `crates/doggy-dns-control/tests/common/mod.rs`
- Modify: root `Cargo.toml` (workspace member), core/plugin/observability `Cargo.toml` (dev-dep), `crates/doggy-dns-core/tests/{common/mod.rs,handler_test.rs,wire_golden_test.rs,middleware_test.rs}`, `crates/doggy-dns-core/benches/handler_bench.rs`, `crates/doggy-dns-plugin/tests/{authority_chain_test.rs,zone_handler_test.rs}`, `crates/doggy-dns-observability/tests/common/mod.rs`, `crates/doggy-dns-nacos/src/test_support.rs` + `tests/{authority_test.rs,mapping_test.rs}` + `benches/nacos_bench.rs`, control tests ×3

**Interfaces (testutil crate):**
- `pub fn dns_request(id: u16, domain: &str, rtype: RecordType, edns: Option<Edns>) -> Request` — MOVE verbatim from `core/tests/common/mod.rs:20-35` (doc comment included). core's common becomes a re-export or is deleted in favor of direct imports.
- `pub struct CapturingHandler` — merge `MockResponseHandler` (handler_test.rs:66-89) and `ByteCapturingHandler` (wire_golden_test.rs:37-57): capture emitted bytes; expose `bytes()` and `message()` (reparse). The golden test's byte expectations must not move — if this merge changes ANY golden byte, the merge is wrong.
- `pub struct MockZoneHandler` — parameterized replacement for the ~50-line mocks (authority_chain_test.rs:66-116, handler_test.rs:92-142): constructor takes a behavior (enum `LookupBehavior { Skip, Continue(AuthLookup), Break(...), Error(LookupError), … }` or a boxed closure `Fn(&LowerName, RecordType) -> LookupControlFlow<AuthLookup>`; closure is simpler — choose closure + `Send + Sync`). Keep zone_type/origin configurable.
- nacos: move `qname()`/`lower()` (authority_test.rs:433-435 ≡ mapping_test.rs:17-19) and `one_instance()`/`instances()` into existing `src/test_support.rs`; `nacos_bench.rs:29-79` hit/miss → one parameterized function over the query name.
- control: `tests/common/mod.rs` with the shared `test_state`/`test_live` (http_contract_test.rs:10-35 ≡ tools_test.rs:10-35 ≡ startup_test.rs:12-23).

- [ ] **Step 1:** full workspace test baseline green (post all earlier merges/branches — G rebases on latest main if the earlier PRs merged; else on I's tip only if stacked, default: fresh from main and re-verify).
- [ ] **Step 2:** create testutil, migrate ONE crate at a time, run that crate's tests after each migration (core → plugin → observability). Golden bytes unchanged is the acceptance criterion.
- [ ] **Step 3:** nacos + control dedup; `cargo bench --no-run --workspace` compiles.
- [ ] **Step 4:** fmt/clippy/test workspace; commit `test: share wire builders, capturing handlers and zone mocks instead of six copies` + trailer.

---

## Self-review notes

- Spec coverage: items 1-2 → A; 3-5 → B (+C for the lock); 6 → D; 7 → E; 8 → F; 9 → G; 10-11 → H; 12 → I. The two downgraded findings (watcher timeout skeleton, observability inline-test relocation) are deliberate non-goals.
- Type consistency: `outcome_attributes -> [KeyValue; 4]` used consistently in B; `compute_cache_entry(&[u8])` callers updated in the same task; testutil signatures defined once in G.
- Known risks flagged inline: A verifier shapes (fallbacks ordered), C Sync bound + measurement gate, E key order, H CI nested build.
