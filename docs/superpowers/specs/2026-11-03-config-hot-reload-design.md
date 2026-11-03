# Config Hot Reload — Detailed Design

**Date:** 2026-11-03
**Status:** Draft for review
**Scope:** Wire `[remote_config]` (Nacos config push) and SIGHUP into one shared
reload pipeline; make the authority chain, middleware set, and
`rotate_answers` hot-swappable; classify everything else as restart-required.

Related: `docs/superpowers/specs/2026-03-14-doggy-dns-phase1-design.md`
(original `NacosConfigWatcher` intent, never wired).

---

## 1. Current state (verified against the working tree)

| Fact | Evidence |
|------|----------|
| `NacosConfigWatcher` is fully written but referenced nowhere | `crates/doggy-dns-nacos/src/config_watcher.rs`; no mention in `src/main.rs` |
| `[remote_config]` is parsed and validated, never applied | `crates/doggy-dns-basic/src/remote.rs`, `validate.rs`; main.rs only logs `remote_config_enabled` |
| Config is loaded once, never again | `src/main.rs` `main()` → `load_config()` |
| `DoggyDnsHandler` owns `Vec<Arc<dyn Middleware>>` + `Arc<AuthorityChain>` by value | `crates/doggy-dns-core/src/handler.rs:57-62` |
| `AnswerRotation.enabled` is a plain `bool` fixed at construction | `handler.rs:31-34` |
| `McpState` is an immutable startup snapshot (`config: ConfigSnapshot`, `plugins: Vec<PluginSnapshot>`) | `crates/doggy-dns-control/src/state.rs:9-19` |
| MCP `get_config` shows the raw, unexpanded startup file content | `ConfigSnapshot::capture` in `control/src/lib.rs` |
| Control-plane start failure is non-fatal by precedent | `main.rs` `start_control_plane` |
| Resolved nacos-sdk is **0.8.3**: naming `unsubscribe` (api/naming.rs:256) and config `remove_listener` (api/config.rs:133) are both public | `Cargo.lock` |
| `doggy-dns-core` depends on nacos only as a dev-dependency | `crates/doggy-dns-core/Cargo.toml` |
| `PluginKind` derives `PartialEq, Eq`; `PluginConfig` does not | `crates/doggy-dns-basic/src/plugin.rs` |
| `parse_config` (expand → deserialise → validate) is private | `crates/doggy-dns-basic/src/load.rs:21` |

## 2. Goals / Non-goals

**Goals**

1. A rejected reload can never crash or degrade the running resolver: whole
   reload is all-or-nothing; the old config stays in force on any failure.
2. Nacos push and SIGHUP feed the same pipeline and the same guarantees.
3. Unchanged plugins keep their live Nacos connections, subscriptions, and
   warm caches across a reload — no cold-cache window.
4. MCP `get_config` / `list_plugins` reflect what is actually in force.

**Non-goals**

- No hot changes to listen sockets, the tokio runtime, worker threads,
  CPU affinity, reuseport, TLS/DoH, XDP, or observability. These are
  **restart-required**: detected, reported, never applied.
- No config history, rollback, or persistence.
- No inotify file watching — SIGHUP is the local trigger (K8s ConfigMap
  workflows and `kill -HUP` both map onto it).
- No restart-required field triggering an automatic process exit.

## 3. Runtime topology after the change

```
                     ┌──────────────────────────────────────────────────┐
                     │                 process (binary crate)           │
                     │                                                  │
 SIGHUP ─────────────┼──► trigger task (cfg unix) ──┐                   │
 (re-read config     │                              ▼                   │
  file from disk)    │                     ┌─────────────────────┐        │
                     │                     │  ReloadCoordinator  │        │
 Nacos config push ──┼──► NacosConfigWatcher│  (src/reload.rs)   │        │
 ([remote_config],   │    (process-lifetime)│                     │        │
  one watcher,       │    + mpsc channel ──►│ apply_reload():     │        │
  singleton)         │                      │  1 identical-skip   │        │
                     │                      │  2 parse+validate   │        │
                     │                      │  3 plugin diff      │        │
                     │                      │  4 build/reuse      │        │
                     │                      │  5 handler.swap()   │        │
                     │                      │  6 restart-required │        │
                     │                      │    diff → warn      │        │
                     │                      │  7 snapshot update  │        │
                     │                      └──────┬──────────────┘        │
                     │                             │ ArcSwap::store        │
                     │                             ▼                       │
                     │  query path ──► DoggyDnsHandler ──► AuthorityChain  │
                     │  (unchanged)    (core/handler.rs)   (old chain      │
                     │                                      drains via Arc)│
                     └──────────────────────────────────────────────────┘
```

XDP is transparent to the swap: kernel cache hits never reach the authority
chain; misses resolve through whichever chain is current. XDP configuration
itself is startup-frozen.

## 4. Reload flow

```
apply_reload(source, raw_content_opt)
 │
 ├─ raw_content_opt is None (SIGHUP)? ── read config_path ── err ⇒ REJECT (error log)
 │
 ├─ acquire reload_lock (tokio::sync::Mutex — serialises SIGHUP vs push)
 │
 ├─ raw == last_applied? ── yes ⇒ info log, DONE (no-op)
 │
 ├─ parse_config(raw)                      [basic, now pub: expand ${VAR} →
 │                                          deserialise → validate_config]
 │        err ⇒ REJECT (error log w/ source) — old config untouched
 │
 ├─ diff plugins (Vec<PluginConfig>, PartialEq)
 │        identical ⇒ keep old chain & watchers, no rebuild
 │        changed   ⇒ rebuild: position-wise reuse of identical entries,
 │                    build new ones, retire the rest (see §6.3)
 │        rebuild err (e.g. Nacos unreachable) ⇒ REJECT whole reload
 │
 ├─ handler.swap_runtime(new_middlewares, new_chain)
 │  rotation.set_enabled(new_config.server.rotate_answers)
 │
 ├─ restart-required diff (toml::Value tree walk over both configs,
 │  hot paths filtered out) ⇒ one warn listing changed paths, if any
 │
 ├─ control plane: update_runtime(config_snapshot, plugin_snapshots)
 │
 ├─ last_applied = raw
 └─ info log: source, applied set, deferred set
```

**Applied set (hot):** `plugins`, `middleware.logging`,
`server.rotate_answers`.
Everything else is restart-required and only reported.

## 5. Module-level changes

### 5.1 `doggy-dns-basic`

```rust
// load.rs — was private; reload reuses it verbatim (expand → parse → validate)
pub fn parse_config(content: &str) -> anyhow::Result<DoggyDnsConfig>;

// plugin.rs — add derives so reload can diff plugin lists
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PluginConfig { ... }        // all fields are String/Option<String>/usize/bool
```

New: every config struct under `basic` derives `Serialize` (they are plain
data; serde is already a dependency). This powers the restart-required diff
without hand-written field lists (§6.4).

### 5.2 `doggy-dns-core` (`handler.rs`)

```rust
pub struct AnswerRotation {
    enabled: AtomicBool,          // was bool
    cursor: AtomicUsize,          // untouched by set_enabled
}
impl AnswerRotation {
    pub fn new(enabled: bool) -> Self;          // unchanged signature
    pub fn set_enabled(&self, enabled: bool);   // new
    pub fn apply(&self, records: &mut [Record]); // reads AtomicBool
}

pub struct DoggyDnsHandler {
    middlewares: ArcSwap<Vec<Arc<dyn Middleware>>>,   // was Vec
    authority_chain: ArcSwap<AuthorityChain>,          // was Arc
    xdp_enabled: bool,                                 // frozen at startup
    rotation: AnswerRotation,
}
impl DoggyDnsHandler {
    pub fn new(... /* signature unchanged */) -> Self; // stores into ArcSwaps
    /// Replace the middleware set and the authority chain in one call. The two
    /// stores are independent, not one atomic pair; the window between them is
    /// invisible to resolution because only the chain is functional.
    /// In-flight queries keep resolving on the old Arc; no lock is held
    /// across resolution.
    pub fn swap_runtime(
        &self,
        middlewares: Vec<Arc<dyn Middleware>>,
        chain: Arc<AuthorityChain>,
    );
}
```

`new()` keeps its current signature, so every existing handler test
(`wire_golden_test`, `alloc_guard_test`, benches) is untouched. The query path
changes only in how it reads the chain/middlewares (`load()` instead of field
access); hot-path cost is one acquire-load per request — the same primitive the
XDP stats path already uses. Add `arc-swap = "1.9"` to workspace deps and core
(1.9.3 is already in `Cargo.lock` transitively).

### 5.3 `doggy-dns-nacos` (`watcher.rs`)

Track subscriptions so a retired watcher can leave cleanly (0.8.3
`NamingService::unsubscribe` is public):

```rust
pub struct NacosServiceWatcher {
    // existing fields...
    subscribed: DashMap<ServiceKey, Arc<InstanceChangeListener>>,  // new
}
impl NacosServiceWatcher {
    pub async fn unsubscribe_all(&self);   // new: drain `subscribed`, call
                                           // naming.unsubscribe per entry;
                                           // log failures, never abort
}
```

`subscribe()`/`subscribe_all()` insert into `subscribed` alongside their
current behaviour. `config_watcher.rs` is unchanged — it already exposes
exactly what the trigger task needs (`new`, `start_watching`, callback with
raw content).

### 5.4 `doggy-dns-control`

`McpState`'s `config` and `plugins` become one swappable runtime snapshot:

```rust
#[derive(Debug)]
pub struct McpState {
    // ...startup-frozen fields unchanged (instance_id, host, pid, version,
    //    started_at, listen_addr, protocols — listen_addr/protocols describe
    //    the *running* server and cannot drift without a restart)...
    runtime: ArcSwap<RuntimeSnapshot>,   // was config + plugins fields
}
pub struct RuntimeSnapshot {
    pub config: ConfigSnapshot,             // re-captured (redacted) per reload
    pub plugins: Vec<PluginSnapshot>,       // rebuilt per reload
}
impl McpState {
    pub fn runtime(&self) -> arc_swap::Guard<RuntimeSnapshot>;  // readers
    pub fn update_runtime(&self, config_content: &str, plugins: Vec<PluginSnapshot>);
}
```

`server.rs` readers (`&self.state.config` at server.rs:116 and the
`list_plugins` path) switch to `state.runtime().config` / `.plugins`.
`build_state()` keeps its signature. The `Clone` derive on `McpState` is
dropped if unused (verified at implementation time); `ConfigSnapshot` keeps
its own `Clone`.

`ControlPlaneHandle` gains the `Arc<McpState>` it already owns internally, so
the reload coordinator can call `update_runtime` after a successful reload.
The update is infallible in-memory work — it cannot fail a reload.

### 5.5 Binary crate: `src/reload.rs` (new)

Everything that needs both core and nacos lives here; the core→nacos crate
edge stays dev-only.

```rust
#[derive(Debug, Clone, Copy)]
pub enum ReloadSource { Sighup, Nacos }   // Display: "sighup" / "nacos"

/// The full built-chain state the coordinator owns, so startup and reload
/// share one builder and reload can reuse surviving entries.
struct BuiltChain {
    plugins: Vec<PluginConfig>,
    handlers: Vec<(String, Arc<dyn ZoneHandler>)>,
    watchers: Vec<(String, NacosServiceWatcher)>,
}

pub struct ReloadCoordinator {
    config_path: PathBuf,
    handler: Arc<DoggyDnsHandler>,
    metrics_middleware: Option<Arc<dyn Middleware>>,  // captured at startup;
        // middlewares rebuilt as [metrics?] + [LoggingMiddleware if enabled]
    control_state: Option<Arc<McpState>>,
    current: Mutex<BuiltChain>,
    last_applied: Mutex<String>,
    reload_lock: tokio::sync::Mutex<()>,
}
impl ReloadCoordinator {
    /// Startup path: build the initial chain and handler state.
    pub async fn init(config, config_path, handler, ...) -> Result<Self>;
    /// The one entry point both triggers call.
    pub async fn apply_reload(&self, source: ReloadSource, raw: Option<String>);
}
```

`build_authority_chain` moves from `main.rs` into `reload.rs` and grows a
reuse parameter:

```rust
async fn build_authority_chain(
    config: &DoggyDnsConfig,
    reuse: Option<&BuiltChain>,     // None at startup; Some on reload
) -> Result<BuiltChain>;
```

**Position-wise reuse rule** (§4 step 3, spelled out): plugins are an ordered
list; entries are matched by index. `old[i] == new[i]` (after the `PartialEq`
derive, `enabled` included) ⇒ carry over `old_handlers[i]` and `old_watchers[i]`
as-is. Any index that differs, or a new suffix, is built fresh. An old suffix
beyond the new length is retired: nacos watchers get `unsubscribe_all()` +
drop; native/forward handlers are just dropped (their Caches drain with the
chain's Arc). Deliberately simple: inserting a plugin in the middle rebuilds
every entry after it — acceptable at reload frequency, documented in §8.

The coordinator also owns the metrics middleware Arc so a reload with
`middleware.logging = false` can still rebuild the middleware vec; the XDP
middleware (if any) is appended by the same rule — it is captured once at
startup, exactly like today.

### 5.6 Binary crate: trigger wiring (`main.rs`)

```rust
// in async_main, after build_server / control plane:

// 1. SIGHUP (cfg unix): loop { sig.recv() ⇒ coordinator.apply_reload(Sighup, None) }
//    The task holds the coordinator; the config path was passed into it at init.

// 2. Nacos push (remote_config.enabled):
//    spawn {
//      loop {
//        match NacosConfigWatcher::new(addr, ns, group, data_id).await {
//          Ok(w) => {
//            let (tx, rx) = mpsc::channel(8);
//            w.start_watching(move |content| { let _ = tx.try_send(content); })
//            break; // watcher + listener live for the process
//          }
//          Err(e) => warn!(%e, "remote config watcher unavailable, retrying in 30s"),
//        }
//        sleep(30s)
//      }
//      while let Some(content) = rx.recv().await {
//        coordinator.apply_reload(Nacos, Some(content)).await
//      }
//    }
```

The synchronous `Fn(String)` callback hands content to the async pipeline via
a bounded mpsc; `try_send` dropping an overflow push is fine — the next push
or a SIGHUP resynchronises, and the identical-content skip makes re-pushes
cheap. Startup connection failure is **non-fatal** (warn + 30 s retry),
following the `start_control_plane` precedent; the resolver serves DNS
throughout. On graceful shutdown these tasks are simply dropped with the
runtime; they hold no shutdown-relevant state. Logging middleware on/off and
rotation are applied even when the plugin list is unchanged.

**Startup notify semantics:** nacos-sdk notifies a freshly registered listener
with the *current* published content. A config already published in Nacos that
differs from the local file therefore applies right after startup, via the
normal reload path (parse → identical-skip fails → hot diff applies). This is
deliberate — Nacos acts as the source of truth once `[remote_config]` is on —
and the e2e test below pins it.

### 5.7 Diff reporting (§4 step 6)

Both the old and new `DoggyDnsConfig` are serialised to `toml::Value`
(requires the new `Serialize` derives), and a small recursive walker collects
differing leaf paths (`server.port`, `observability.endpoint`, …). Hot paths —
`plugins`, `middleware.logging`, `server.rotate_answers` — are filtered out
before reporting. Any remaining difference produces a single `warn`:

```
reload applied; restart required for: server.port, server.tls
```

A restart-required-only reload (identical hot set) still succeeds: it applies
nothing, updates nothing, but records `last_applied = raw` so the same content
is not re-diffed on every push.

## 6. Failure matrix

| Source | Failure | Behaviour | Log |
|--------|---------|-----------|-----|
| SIGHUP | config file unreadable | reject, keep old | `error` (source, path, error) |
| both | parse / `${VAR}` missing / validate fails | reject, keep old | `error` (source, error) |
| both | chain rebuild fails (e.g. Nacos unreachable) | reject **whole** reload, keep old | `error` (source, error) |
| Nacos | watcher connect fails at startup | non-fatal; retry every 30 s | `warn` per retry |
| Nacos | push identical to applied | skip | `info` |
| both | only restart-required fields differ | nothing applied | `info` applied-nothing + `warn` paths |
| both | hot fields differ | swap + toggle + snapshot update | `info` summary |

A rejected reload leaves zero observable state changes: same chain, same
watchers, same snapshot, same `last_applied` (so a corrected push is not
skipped).

## 7. Concurrency model

- `reload_lock` serialises reloads; a SIGHUP arriving during a Nacos-triggered
  reload queues behind it (trigger tasks are independent, so a burst cannot
  block the query path).
- `last_applied` is only read/written under `reload_lock`.
- `ArcSwap::store` is wait-free; queries never wait on a reload. In-flight
  queries hold the old `Arc<AuthorityChain>`; retired watchers' caches die
  with the chain's last reference.
- `AnswerRotation` cursor is never reset on toggle: rotation on/off preserves
  spreading continuity, and existing tests pin cursor semantics.

## 8. Known limitations (accepted, documented)

1. **Position-wise plugin diff** — inserting/removing a plugin in the middle
   rebuilds all later plugins (fresh Nacos connections, cold caches for those
   plugins only). Rare operation; LCS-style diffing not worth it now.
2. **Retired Nacos connections linger** — nacos-sdk 0.8.3 has no
   `NamingService` shutdown; subscriptions *are* removed server-side via
   `unsubscribe`, but the retired client's TCP connection lives until process
   exit. Reload frequency makes this moot.
3. **`[remote_config]` changes need a restart** — 0.8.3 could actually remove
   a config listener (`remove_listener` is public), but rebuilding the watcher
   on its own section change adds reconnect/consistency complexity for a
   field nobody changes mid-flight. Classified restart-required by choice.
4. **MCP server info fields are startup-frozen** (`listen_addr`, `protocols`)
   — they describe the running server; restart-required config changes never
   make them stale.
5. **XDP cache order freeze** — unchanged existing behaviour; a cache hit
   replays the order captured at admission until TTL expiry (warn at startup
   already covers it).
6. **A pushed configuration lands on the SDK's poll, not on publish** —
   measured against standalone Nacos 2.5.4 with nacos-sdk 0.8.3: the change was
   noticed by `list_ensure_cache_data_newest` (every 60 s) and no
   `ConfigChangeNotifyRequest` ever reached the client, although naming pushes
   over the same connection do arrive. Worst-case propagation is therefore a
   minute; `SIGHUP` is the immediate trigger. `tests/nacos_e2e_test.rs` waits on
   `PUSH_DEADLINE` (120 s) because of this, and the invalid-push case waits for
   the resolver's own rejection log line rather than a blind sleep, so the bad
   content is proven delivered before anything asserts it changed nothing.

## 9. Test plan

**core (`crates/doggy-dns-core/tests/` or inline per repo convention)**

- `swap_runtime`: stub `ZoneHandler` A in the chain; issue a query (hits A);
  swap to chain B; issue a query (hits B). With tokio join, start a slow query
  on A, swap mid-flight, assert the in-flight answer still came from A.
- `set_enabled`: toggle on/off around multi-record answers; assert the cursor
  continues (no reset) across the toggle.

**reload pipeline (binary `tests/reload_test.rs`)**

- applied: temp TOML, change `rotate_answers` + `middleware.logging` ⇒ applied
  log semantics; handler reflects the toggle.
- rejected-bad-toml: invalid TOML ⇒ coordinator state unchanged (chain,
  `last_applied`).
- rejected-bad-validate: e.g. `worker_threads = 0` ⇒ same.
- identical-skip: re-apply the same content ⇒ no rebuild (observable via a
  counter injected into the builder, or via watcher identity).
- restart-required-only: change `server.port` ⇒ no chain rebuild, warn path.
- plugin diff unit tests: `plan`-style pure function tested for
  reuse/rebuild/retire permutations (same-prefix, middle-insert, shrink,
  param-change).

**nacos (`crates/doggy-dns-nacos/tests/`)**

- `unsubscribe_all` removes exactly the keys recorded by
  `subscribe`/`subscribe_all` (mock level, per repo convention: no live Nacos
  in unit tests).

**control (`crates/doggy-dns-control/tests/state_test.rs`)**

- `update_runtime` then read back: new content and plugin snapshots visible;
  redaction still applied to the new content.

**Live-Nacos e2e (`tests/nacos_e2e_test.rs`, `#[ignore]`, runs in
`.github/workflows/nacos-ci.yml` like the existing cases)**

Mirrors the established pattern — the test process owns a Nacos SDK client and
spawns the real binary, assertions poll real DNS answers:

- `config/doggy-dns-nacos-e2e.toml` gains `[remote_config]` (`enabled = true`,
  `server_addr = "${DOGGY_NACOS_ADDR}"`, a per-case `data_id`).
- Each case first publishes the full case config (port-patched like
  `DoggyDns::case_config` does today) via
  `ConfigService::publish_config(data_id, group, content)`, *then* starts
  doggy-dns — the startup-notify semantics (§5.6) make the published config
  the one in force.
- `e2e_config_push_reloads_plugins`:
  1. publish config with nacos plugin `ttl = 6`; register an instance; await
     answers carrying `ttl == 6`.
  2. publish the same config with `ttl = 3`; `poll_until` answers carry
     `ttl == 3` — proves push → parse → chain rebuild took effect without a
     restart (TTL is the observable because it rides every answer).
  3. publish invalid TOML; re-query: the server must still answer with
     `ttl == 3` — proves a rejected push leaves the old config in force and
     never crashes the process.
- The case registers its instances before publishing/starting, same as the
  existing cases; the SDK `ConfigService` handle lives in the test process,
  so published configs vanish with it.

**Gates:** `cargo fmt --all -- --check`, `cargo clippy --all-features
--all-targets -- -D warnings`, `cargo test --workspace`,
`cargo bench --no-run --workspace` (bench boilerplate compiles unchanged
since `new()` signatures stay). The e2e case additionally gates on the CI
Nacos job (locally: the Podman Nacos run documented at the top of
`nacos_e2e_test.rs`).

## 10. Implementation order

1. basic: `parse_config` pub, `PluginConfig` `PartialEq, Eq`, `Serialize`
   derives.
2. core: `ArcSwap` handler + `AnswerRotation` atomic (+ tests).
3. nacos: subscription tracking + `unsubscribe_all` (+ test).
4. control: `RuntimeSnapshot` + `update_runtime` (+ test).
5. binary: `reload.rs` (coordinator, builder with reuse, diff walker) +
   trigger wiring in `main.rs`.
6. binary integration tests + live-Nacos e2e case (`e2e_config_push_reloads_plugins`
   + `[remote_config]` in the e2e config) + docs (`docs/configuration.md` short
   section: hot vs restart-required, SIGHUP usage, `[remote_config]` semantics).
7. Full gate run, then ship as one PR (one reviewable unit; spec + plan docs
   included, matching repo precedent).

## 11. Decisions made in this design (vetoable)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | remote-config watcher startup failure is non-fatal (warn + 30 s retry) | auxiliary control source; control plane already non-fatal; the resolver must serve DNS without it |
| D2 | unchanged plugin entries reuse the watcher *object* (connection + subscriptions + cache) | avoids cold-cache NXDOMAIN windows; the common reload (e.g. toggling forward upstream) touches nothing Nacos |
| D3 | whole-reload atomicity, no partial degradation | predictable; a half-old half-new chain is a debugging trap |
| D4 | restart-required diff via `Serialize` + toml tree walk | no hand-maintained field lists; hot paths filtered by name |
| D5 | position-wise plugin diff | simplest correct rule; middle-insert churn accepted (§8.1) |
| D6 | SIGHUP instead of inotify for the local trigger | K8s + ops convention; no extra dependency |
| D7 | `[remote_config]` changes are restart-required despite SDK support for re-listening | rebuild complexity not justified for a section nobody changes mid-flight |

## 12. Implementation record (what changed once the code existed)

The §5 sketches were written before the builder had to be testable; the shipped
shape differs in names and in one owned value, and the live run added one fact
and one validation rule.

- **`BuiltChain` became `PluginSlot`** (`src/reload.rs`). One slot per
  `[[plugins]]` entry: `name`, `handler`, optional `watcher`. `build_chain` takes
  `&mut Vec<PluginSlot>` (the previous slots), fills the new list, then commits
  each reused slot by moving its parts out — so reuse is position-wise *and*
  visible to tests: `tests/reload_test.rs` proves it with `Arc::ptr_eq` on
  `PluginSlot::handler()` instead of a counter injected into the builder.
  `authority_chain_from(&slots)` and `middlewares_for(logging, base)` are separate
  so a test can build a chain without a handler.
- **`ReloadCoordinator::init` became `new` + the startup call in `main.rs`.** A
  constructor that `async`-built the chain could not report the build error the
  same way `apply_reload` does; `main.rs` calls `build_chain` once with an empty
  previous list and hands the slots over. Same code path, one fewer special case.
- **`metrics_middleware: Option<Arc<dyn Middleware>>` became
  `base_middlewares: Vec<Arc<dyn Middleware>>`.** Two middlewares survive every
  reload — observability's metrics middleware and the XDP cache middleware
  `xdp::init` pushes — so the reload composes `LoggingMiddleware` onto whatever
  the base set is instead of assuming one fixed partner.
- **`apply_reload` returns `ReloadOutcome`** (`Applied`/`Skipped`/`Rejected`).
  The design left it `()`; a returned outcome is what lets the pipeline tests
  assert the four paths instead of reading logs. The triggers themselves ignore
  it — every branch already logs — so no `Display` impl ships.
- **`LiveState` gained `replace_nacos_zones` and `LiveProbe`.** The design missed
  that `/readyz` and `get_runtime_status` captured the *startup* watcher `Arc`s,
  so a reload would have reported caches nothing owns. The zone signals are now
  re-published from the surviving slots on every applied reload
  (`nacos_zone_signals(&slots)`), and `LiveState` is interior-mutable
  (`RwLock<Vec<LiveProbe>>`) so `main.rs` keeps passing `&Arc<LiveState>` around
  unchanged.
- **`remote_config.group` is validated non-empty.** nacos-sdk's `add_listener`
  calls `check_not_blank(group)`, so an enabled trigger without a group would have
  retried forever with a `warn` instead of failing at startup. Covered by
  `crates/doggy-dns-basic/tests/config_test.rs::remote_config_enabled_missing_group_fails`.
- **Four e2e cases, not one.** `e2e_config_push_reloads_plugins` (push changes the
  answer TTL; also asserts the rebuilt authority re-reads the instances and that
  the retired watcher logs `unsubscribed from all nacos services`) and
  `e2e_invalid_config_push_keeps_the_running_config` (a rejected push leaves the
  configuration in force, the process alive, and the watcher still listening).
  Both start their resolver with stdout/stderr redirected to a log file and wait
  for a line to appear there: a rejected push is invisible in DNS by contract, so
  the log is the only proof the bad content arrived — and publishing anything else
  before that line lets the SDK poll report only the newest content, so the
  invalid push would never reach the pipeline at all.
  `e2e_sighup_reloads_the_config_immediately` covers the file trigger with no
  `[remote_config]` in its config, so nothing but the signal can be credited with
  the flip, and waits on `SIGHUP_DEADLINE` (10 s, less than one 60 s SDK poll) for
  the answer to change — that is what makes it a case about `SIGHUP` rather than
  the push path again. `worker_threads` rides along as the field it must report
  as needing a restart instead of pretending to apply.
  `e2e_push_without_plugin_changes_keeps_the_warm_cache` covers D2's reuse
  promise: a push that changes only `middleware.logging` must not touch the
  authority, so the case samples the zone across the reload and requires zero
  blips, no retire line in the log, and `logging=true` in the applied line — plus
  one later query that produces `dns query completed`, which only the newly
  installed `LoggingMiddleware` writes.
- **Colour escapes were a live failure, and its fix is on both sides.** The first
  live run of the two new cases failed while the four older ones passed: the fmt
  layer colourised even into a file, so a field pair landed as
  `\x1b[3mlogging\x1b[0m\x1b[2m=\x1b[0mtrue` and the needles `logging=true` and
  `config reload applied source=sighup` could never match, while message-text
  needles did. The resolver now colourises only when its stdout is a terminal
  (`observability::install_subscriber`), which is the production-side answer — no
  log file or shipper should inherit escape codes. `strip_ansi` stays in the test
  as a guard, with `strip_ansi_leaves_field_pairs_searchable` (not `#[ignore]`d)
  checking the collapse against a captured colourised line.
- **Case helpers collapse onto two functions.** Every configuration variant a
  reload case needs is one line of the committed file rewritten
  (`patch_line`), and every push is a publish on the case's own data id
  (`DoggyDns::publish`); `patched_config`, `with_ttl`, `with_logging`,
  `with_worker_threads` and the four publish sites are thin calls over them.
- **The gap check samples past the `applied` line.** `apply_reload` swaps the
  chain and only then logs that it applied, so the empty-cache window a rebuilt
  authority opens starts *before* the line a waiter would stop at. Sampling
  continues for `WARM_SAMPLES_AFTER` more queries, and an unanswered query counts
  as a blip too — otherwise a blackout would look like reuse.
- **The implicit default chain is reused, like any other entry.** An early cut
  skipped reuse whenever `[[plugins]]` was absent, which contradicted what
  `synthetic_default_forward` documents and rebuilt (and cooled) the default
  forwarder on every reload of a zero-config process. Equality alone is safe
  across that boundary because the implicit entry's tuning (`cache_size 1024`,
  `min_ttl 30`, `max_ttl 600`) differs from the serde defaults an explicit entry
  gets, so the two can never compare equal; both halves are asserted by
  `no_plugins_still_builds_the_default_forward_chain`.
- **A rebuilt nacos authority is briefly empty.** `subscribe_all` establishes the
  subscription and the first instance list arrives a moment later as an ordinary
  change event (~0.5 s in our runs, never asserted as a bound). Inherent to D2's
  "changed entries get rebuilt", and the e2e case waits for it rather than seeding
  the cache by hand.

- **The §9 mock-level `unsubscribe_all` test was dropped.** The teardown calls
  `NamingService::unsubscribe` directly, so there is nothing to observe without a
  server; adding a `subscribed_services()` accessor to `NacosServiceWatcher` would
  have been test-only API. The live e2e reload cases cover it instead — their
  process logs show `unsubscribed from all nacos services` on the retire path.

Live result against the Podman `nacos-server:v2.5.4` recipe at the top of
`tests/nacos_e2e_test.rs`: 8 of 8 ignored cases pass in 123.5 s. Each push case's
own log file shows the whole path — `config reload listening on nacos push`, then
`config reload applied source=nacos` (and, for the invalid one,
`config reload rejected: invalid configuration, keeping the one in force
source=nacos` first), then `unsubscribed from all nacos services` when the ttl
change retires the watcher.

The two reload-specific logs say what §8's D2 promise is worth in a live process:

- the `SIGHUP` case applied at `04:07:14.756`, one second after the process
  reported `config reload listening on SIGHUP` and ~60 s before a poll could have
  delivered anything, with `paths=["server.worker_threads"]` reported for the
  field it cannot apply;
- the warm-cache case applied `logging=true` and never logged a retire line, while
  the `SIGHUP` case's own timeline shows why that absence matters: its swap left
  the zone unserved until `nacos service changed: e2e-sighup` 641 ms later, so a
  rebuild under the warm case's ~135 ms sampling would have shown several blips.
