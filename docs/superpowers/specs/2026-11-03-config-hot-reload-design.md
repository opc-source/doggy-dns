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
   plugins only). Rare operation; LCS-style diffing not worth it now. §13
   narrows this: the handlers still shift, but a later entry pointing at the same
   `server_addr`/`namespace`/`group` keeps its connection and its cache, because
   reuse is matched by identity rather than by position.
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
- **Four e2e cases, not one.** `e2e_config_push_reloads_plugins` (§13 replaced this
  case: a TTL push now reuses its connection instead of rebuilding and retiring) and
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
  the cache by hand. §13 leaves this only to entries that move the connection.

- **The §9 mock-level `unsubscribe_all` test was dropped.** The teardown calls
  `NamingService::unsubscribe` directly, so there is nothing to observe without a
  server; adding a `subscribed_services()` accessor to `NacosServiceWatcher` would
  have been test-only API. The live e2e reload cases cover it instead — their
  process logs show `unsubscribed from all nacos services` on the retire path.

Live result against the Podman `nacos-server:v2.5.4` recipe at the top of
`tests/nacos_e2e_test.rs`: 8 of 8 ignored cases pass in 123.5 s (the re-run after
§13's port: 9 of 9 in 123.00 s). Each push case's own log file shows the whole
path — `config reload listening on nacos push`, then
`config reload applied source=nacos` (and, for the invalid one,
`config reload rejected: invalid configuration, keeping the one in force
source=nacos` first), then `unsubscribed from all nacos services` when the ttl
change retires the watcher — §13 removed that last part, since a ttl push now
keeps its connection.

The two reload-specific logs say what §8's D2 promise is worth in a live process:

- the `SIGHUP` case applied at `04:07:14.756`, one second after the process
  reported `config reload listening on SIGHUP` and ~60 s before a poll could have
  delivered anything, with `paths=["server.worker_threads"]` reported for the
  field it cannot apply;
- the warm-cache case applied `logging=true` and never logged a retire line, while
  the `SIGHUP` case's own timeline showed why that absence mattered *in this
  round*: its swap left the zone unserved until `nacos service changed:
  e2e-sighup` 641 ms later, so a rebuild under the warm case's ~135 ms sampling
  would have shown several blips. §13's identity reuse is the answer to that gap.

## 13. Second round: what PR #33 got right, and what this branch took from it

PR #33 implements the same feature on a competing branch. Audited side by side
(its `src/reload.rs`, `crates/doggy-dns-basic/src/reload.rs`,
`crates/doggy-dns-nacos/src/pool.rs`), four of its ideas are better than what §12
shipped, and this round ports them. What §12 relies on stays: unsubscribe on
retire, skip when nothing changed, restart-required fields reported while the hot
ones apply, live-signal refresh after each reload, and deadlines measured against
the SDK's poll (`PUSH_DEADLINE` 120 s, `SIGHUP_DEADLINE` 10 s). #33 has none of
those — `unsubscribe` appears nowhere in its `src/` or crate sources, unpooled
watchers "stay pooled" forever, an identical push still rebuilds the chain, and a
push that touches `worker_threads` is rejected whole, so its hot fields never
land; its single `DEADLINE = 60 s` equals the poll period nacos-sdk 0.8.3 needs to
notice a config change, which makes its own e2e cases flaky by construction.

- **Reuse by connection identity.** `NacosIdentity { server_addr, namespace,
  group }` (`src/reload.rs`) is the reuse key, with `dns_zone` and `ttl`
  deliberately outside it — neither belongs to the naming connection. So the
  change operators actually make, a TTL push, keeps the connection *and* the warm
  `DashMap`, and only the `NacosAuthority` struct above that cache is rebuilt. §8
  and §12's "a rebuilt authority is briefly empty" now apply only to a real
  identity change. Unlike #33's shared pool, the watcher stays owned by the slot
  that reuses it: exactly one owner, so `unsubscribe_all` keeps its meaning and
  the retire path stays honest. Reuse logs `nacos connection reused` and does not
  re-subscribe.
- **Planning as a pure function.** `plan_slots(desired, previous) -> Vec<SlotPlan>`
  returns `Carry(i)` (same position, exact `PluginConfig` equality),
  `ReuseConnection(j)` (identity match anywhere, consumed on match so two plugins
  on one server take two connections) or `Build`. D5's position rule survives for
  the handlers; only connections are matched by identity. Ten unit cases test the
  decision without a server, where before it was observable only through
  `Arc::ptr_eq` on a chain that had to be built first — and one of them found a
  bug on the way: taking the first free connection and checking its identity only
  afterwards left the second plugin of a reordered pair with a fresh connection
  (`reordered_connections_go_to_their_own_plugins`).
- **Debounce and generation.** Both triggers send `ReloadRequest` on an unbounded
  channel and `run_reload_loop` folds a burst newest-wins over `RELOAD_DEBOUNCE`
  (500 ms); `Applied.generation` counts applied reloads — a skip or a rejection
  does not move it — and rides on the applied and restart-warning lines.
- **A truthful baseline.** §12 recorded the whole pushed configuration as the
  baseline, so a restart-required field was reported once and then looked applied
  forever. `adopt_hot_paths_only` moves only the hot paths into the baseline, so
  every later reload still reports the field it cannot apply
  (`a_restart_required_change_is_reported_and_kept_out_of_the_baseline`). Shape is
  part of the same rule: `keep_hot_only` matches tables key by key, so a section
  that exists in the pushed config only — an added `[server.tls]`, whose `Option`
  field is absent from the in-force tree — never enters the baseline
  (`a_section_the_process_never_opened_stays_out_of_the_baseline`), while a hot
  path is adopted whole even where the old tree had none
  (`the_first_plugins_entry_moves_into_the_baseline`).

Verification after the port: `cargo fmt --all -- --check`, `cargo clippy
--all-features --all-targets -- -D warnings`, `cargo test --workspace` and `cargo
bench --no-run --workspace` exit 0; `tests/reload_test.rs` is at 30 cases; live
against the same Podman `nacos-server:v2.5.4`, 9 of 9 ignored cases pass in 123.00 s.

The four per-case process logs are what those claims rest on:

- `e2e_push_ttl_only_reuses_the_nacos_connection` (15415): `nacos connection
  reused plugin=nacos[nacos.e2e.test] addr=127.0.0.1:8848` at `05:58:27.338`,
  `config reload applied source=nacos generation=1` under a millisecond later, no `unsubscribed
  from all nacos services` in the file, and the zone sampled across the swap with
  zero blips before the answer takes the pushed TTL;
- `e2e_sighup_reloads_the_config_immediately` (15417): the same reuse line, applied
  0.97 s after `config reload listening on SIGHUP`, with
  `paths=["server.worker_threads"]` reported for the field it left alone;
- `e2e_push_without_plugin_changes_keeps_the_warm_cache` (15418): applied with
  `logging=true` and neither a reuse nor a retire line — the entry was carried, not
  rebuilt;
- `e2e_push_a_different_server_addr_retires_the_old_connection` (15419):
  `unsubscribed from all nacos services` on the same millisecond as the applied
  line, no reuse line, `paths=["remote_config.server_addr"]` reported, and the new
  `localhost:8848` subscription reporting the instances 507 ms later — §12's gap,
  now confined to the reload that actually moves the connection.

## 14. Third round: `SIGHUP` dropped, and what a push leaves on disk

The two triggers became one. `SIGHUP`'s re-read of the startup file is gone —
`ReloadSource`, `ReloadRequest` and the `std::fs::read_to_string` branch inside
`apply_reload` with them — so `[remote_config]` is the only way a running process
changes its configuration, the channel carries a `String`, and `apply_reload(&str)`
has one path. Two reasons behind the call: a container that takes its configuration
from Nacos has no operator to signal it, and the file trigger re-read a file the
push path never updated, so the two could disagree about what is in force.

**`SIGHUP` is still registered — to be ignored.** With no handler the signal gets
its default disposition, and `kill -HUP` is what an operator reaches for out of
bind/unbound/logrotate habit; it would end a DNS resolver with no drain.
`ignore_sighup` (`src/main.rs`) keeps it caught and logs `SIGHUP ignored`.

**What is in force is now recorded.** Until this round the pipeline kept a
truthful baseline in memory and nothing on disk: a process that booted from an old
file and took twelve pushes would boot from the old file again.
`write_online_record` (`src/reload.rs`) closes that as a record, not an input.

- **Which text.** The pushed text verbatim, restart-required fields and all, so the
  next boot applies the whole of it rather than only what a live socket could take.
  `${VAR}` references stay unexpanded — that is the text the pipeline holds, the
  same text the identical-content guard compares. Startup's record is the
  unexpanded file text `load_config` returned, written once from `async_main`.
- **Where.** `<config-stem>.online.toml`, beside the file the process was started
  from, and never read at startup: the path on the command line stays the only
  input, so a bad remote push cannot become what the process boots into.
- **How.** Three renames, never one write — text to `<record>.tmp` with `sync_all`,
  the previous record to `<record>.bak.<epoch secs>`, then the temp into place — so
  an interrupted run leaves either the old record or the new one. The temp takes the
  old record's permission bits where the platform allows, and if the last rename
  fails the backup is moved back before the `error` is logged.
- **How many.** Ten generations (`ONLINE_BACKUPS_KEPT`), pruned by the stamp in the
  name. The stamp comes from `unix_now_secs` (`doggy_dns_basic::time`, reached here
  as `doggy_dns_core::config::time`): no chrono in the binary for a filename
  suffix. A second push inside the same second advances the stamp, so two reloads
  keep two files rather than one clobbering the other.
- **When it does not happen.** Only `Applied` records. A `Skipped` push is the text
  already in force and a `Rejected` one never became anything, so both leave the
  record and its backups alone. A write failure is a `warn`: the reload it describes
  already swapped the chain, and a read-only directory is not a reason to undo it.

Both the record's status (a record, not the new startup file) and the timestamped
multi-generation backup came from review.

`tests/reload_test.rs` is at 34 cases. Five are the record: pushed text in place
with the previous version in exactly one backup; two pushes in one second keeping
two backups; twelve pushes leaving ten; a rejected push and a skipped push leaving
both the text and the backup count alone; and a record whose directory does not
exist still returning `Applied` with no file left behind. Each was red-checked
against a mutated writer — a no-op `write_online_record` fails the first, second,
third and fifth, removing the prune fails the ten case at 12, skipping the move
aside fails the three that count backups, and recording before the parse fails the
rejected/skipped case.

`tests/nacos_e2e_test.rs` keeps nine ignored cases; the `SIGHUP` one is now
`e2e_pushed_config_is_recorded_and_sighup_changes_nothing` (15417). It reads the
boot record back (equal to the case config, no backup yet), publishes a TTL push,
then asserts the record holds the pushed text while the case config still holds the
startup text — the record never overwrites its input — with that startup text in
exactly one backup. Then `kill -HUP`: the log line, a live process, the pushed TTL
still the answer, and the same one backup.

Verification after this round: `cargo fmt --all -- --check`, `cargo clippy
--all-features --all-targets -- -D warnings`, `cargo test --workspace` (350 passed,
0 failed over 54 result lines) and `cargo bench --no-run --workspace` exit 0; live
against the same Podman `nacos-server:v2.5.4`, 9 of 9 ignored cases pass in 123.12 s.

The 15417 process log is the witness for both halves:

```text
02:55:41.850  INFO doggy_dns: config reload listening on nacos push data_id=doggy-dns-e2e-15417.toml
02:56:42.361  INFO doggy_dns::reload: config reload applied generation=1 plugins=1 logging=false rotate_answers=true
02:56:42.484  INFO doggy_dns: SIGHUP ignored: configuration changes are pushed by the remote config
```

The applied line arrives 60.5 s after the trigger subscribed — §12's poll, now the
only path into the pipeline, with no `source=` field to distinguish it from a file
re-read because there is no file re-read. On disk in `target/tmp` beside the case
config: `nacos-e2e-15417.online.toml` holding the pushed text and
`nacos-e2e-15417.online.toml.bak.1793761002` holding the startup text. The same run
left a record for every other case's process too (15411–15419), which is the
startup write happening in the real binary rather than in a test.

## 15. Fourth round: the trigger moves to the crate that speaks the protocol

`spawn_remote_config_trigger` and `connect_remote_config` left `src/main.rs` for
`spawn_config_pushes` and `connect` in `crates/doggy-dns-nacos/src/config_watcher.rs`.
The binary now holds one line of wiring — build the reload channel, hand it and
`[remote_config]` to the backend, `spawn_config_pushes(config.remote_config.clone(),
reload_tx)` — and the connect/retry/park sequence lives beside the SDK types it
calls. Behaviour is unchanged: same 30 s retry, same watcher parked for the life of
the process, same two log lines.

The move costs no dependency: `doggy-dns-nacos` already declares basic, tokio,
anyhow and tracing. It does cost a `RemoteConfig` clone, because `tokio::spawn`
needs `'static` — the borrowed form fails E0521, and the binary was cloning the
same value before the move.

**Not a trait.** The question was whether the remote-config *abstraction* belongs
in `doggy-dns-plugin`; the answer for now is the code move only. The contract is
already as small as it can be (`NacosConfigWatcher`'s listener takes
`Fn(String) + Send + Sync`), there is one backend, and the plugin crate's own
`AuthorityPlugin`/`registry` still has no implementation — a second axis of
unimplemented abstraction would be debt, not reach. A `ConfigSource` trait earns
its keep when a second backend exists, and `doggy-dns-plugin` is the right host
for it then (`Middleware` already shows the crate holds non-DNS contracts).

One visible consequence: the two nacos-side lines — `config reload listening on
nacos push`, `received config update from nacos` — now carry the
`doggy_dns_nacos::config_watcher` target. §14's transcript is the run before the
move. The e2e cases match on message text, so they never noticed.

Verification after the move: `cargo fmt --all -- --check`, `cargo clippy
--all-features --all-targets -- -D warnings` (no warning lines), `cargo test
--workspace` (350 passed, 0 failed over 54 result lines) and `cargo bench --no-run
--workspace` exit 0; live against the same Podman `nacos-server:v2.5.4`, 9 of 9
ignored cases pass in 123.03 s, with the listening line read back from a case log:

```text
05:34:02.724  INFO doggy_dns_nacos::config_watcher: config reload listening on nacos push data_id=doggy-dns-e2e-15417.toml group=DEFAULT_GROUP
05:35:02.724  INFO doggy_dns_nacos::config_watcher: received config update from nacos data_id=doggy-dns-e2e-15417.toml
05:35:03.226  INFO doggy_dns::reload: config reload applied generation=1 plugins=1 logging=false rotate_answers=true
```

The applied line still arrives 60.5 s after the trigger subscribed, and the record
still lands beside the case config (`nacos-e2e-15417.online.toml`, one backup
`…bak.1793770503`): the pipeline and its record are untouched by where the
subscription lives.

## 16. Fifth round: an unreachable Nacos costs neither the resolver nor a client

The objection was about ownership: `NacosConfigWatcher` holds a `ConfigService`,
which *is* a long-lived gRPC client, so `spawn_config_pushes` constructing a new
one on every failed attempt was wrong. Reading nacos-sdk 0.8.3 said the retry loop
was worse than churn. `FailoverConnection::new` spawns its work task and its health
check before the health check gets to fail (`nacos_grpc_connection.rs:701-720`,
then `nacos_grpc_client.rs:353-375`), and `ConfigWorker::new` spawns
`notify_change_to_cache_data` and `list_ensure_cache_data_newest`
(`config/worker.rs:79-89`) with nothing to stop them. Dropping the client stops the
health check (`FailoverConnection`'s `Drop`, `nacos_grpc_connection.rs:819-826`)
and not those two loops: every failed 30 s attempt left SDK tasks behind.

**The fix is the SDK's own switch, not a retry policy.**
`config_load_cache_at_start` / `naming_load_cache_at_start`
(`api/props.rs:23-25`, setters 260/266, each also overridable through
`ENV_NACOS_CLIENT_{CONFIG,NAMING}_LOAD_CACHE_AT_START`, `props.rs:109-128`) feed
`emergency_start` on the gRPC client (`config/worker.rs:73`, `naming/mod.rs:150`):
when the health check inside `build()` fails, the client logs at `warn` and comes
up anyway. An unreachable Nacos therefore stops being a construction failure —
which was the loop's only reason to exist. Both clients here now set the flag for
the service they build: `config_watcher.rs` for the trigger, `watcher.rs` for
naming. `spawn_config_pushes` does one connect, one subscribe, then parks; a
failure at that point is a broken configuration rather than a missing server, and
the `warn` says `remote config watcher unavailable; this process has no reload
trigger` instead of retrying.

**What changes on the paths that bite.**

- Boot with Nacos down: `build_chain`'s error no longer propagates out of
  `async_main` (`src/main.rs:123` — this boot was fatal), `subscribe_all` keeps its
  existing soft failure (`failed to list nacos services… will rely on dynamic
  subscriptions`), and the zone is served from an empty cache. The wait moves to
  the SDK's backoff, not the socket: refused address, `doggy-dns is ready` 11 s
  after the first health check.
- Recovery is the SDK's: naming re-sends every `SubscribeServiceRequest` through
  its redo task, which is registered *before* the first request
  (`naming/mod.rs:514-520`), runs on a 3 s scheduler (`redo/mod.rs:37-65`) and is
  also fired by the connected listener (`mod.rs:133-138`); config recovers on the
  60 s `list_ensure_cache_data_newest` round (`config/worker.rs:360-407`), whose
  listen contexts come from the cache — and `add_listener` inserts the `CacheData`
  whether or not the server answered (`worker.rs:291-333`), so a subscription that
  started against a dead address is in the very next round.
- Reload with a nacos server that is not answering: `build_chain` succeeds, so the
  push is applied rather than rejected. `an_unreachable_nacos_still_builds_its_slot`
  pins that; `a_plugin_that_cannot_be_built_leaves_the_previous_slots_untouched`
  keeps the partial-consumption invariant on what is left of that door — an address
  every field of which is non-blank, so the config validates, and which the SDK
  still cannot turn into a server list (`server_addr = ","`, rejected by
  `parse_server_list` before any connection is attempted).

**What the cache half of the flag means here**, because the switch is
`load_cache_at_start` and not merely "tolerate a failed health check": the same
flag loads a disk snapshot from `{cache_dir | $HOME/nacos}/…`
(`config/worker.rs:35-39`, `common/cache/mod.rs:26-35`).

- A disk-loaded `CacheData` carries its persisted fields only — `initializing`,
  `listeners` and `config_filters` are not among them (`config/cache.rs:23-30`), so
  an encrypted data id would be delivered undecrypted after a reload-from-disk. We
  register no filter and set no `cache_dir`; noted as a premise, not a bug.
- `get_config` prefers a non-empty cache entry and does not validate it against the
  server (`config/worker.rs:111-121`), so with the flag on a disk snapshot can be
  read back as current. Nothing on the reload path calls it — the push is the only
  input — but `NacosConfigWatcher::get_current_config` stays as the API's reader,
  with this as what a caller has to know about it.
- Naming's snapshot is the one that bites, and it bit during this round's own live
  run: eight of the ten cases timed out with *no answers at all*. The reason is that
  the flag is not only a boot switch — `disk_store` is always on
  (`common/cache/mod.rs:221-233`, defaulting to `$HOME/nacos/<module>/<namespace>`) and
  `load_cache_at_start` fills the in-memory cache from it
  (`naming/mod.rs:95-98`, `cache/mod.rs:26-40`). Every emit then compares against
  that snapshot: `ServiceInfoEmitter::process_service_info` notifies a listener only
  when `is_changed_service_info` says the hosts differ
  (`service_info_observable.rs:169-194`). A resolver that restarts against instances
  nobody touched gets a subscription, a push, and no event — the DNS-facing `DashMap`
  (written only by `InstanceChangeListener::event`) stays empty and the zone answers
  nothing until an instance actually changes. The observable does not replay a cache
  entry to a listener at subscribe time either
  (`service_info_observable.rs:97-112`).

**The subscription therefore reads what it subscribed to.** `NacosServiceWatcher::subscribe`
calls `get_all_instances(service, group, [], subscribe = true)` right after registering
the listener (`watcher.rs`) and writes the result through the same function a push uses,
so one code path owns the healthy/enabled filter, the address parse and the whole-value
replacement. The read is local whenever the SDK holds the service — cache hit, no round
trip — and otherwise sends a `SubscribeServiceRequest` itself
(`naming/mod.rs:346-373`), so the first answers arrive at subscribe time rather than on
the first change. Calling `subscribe_async` a second time is bounded: the redo executor
is a map keyed by `task_key` and `add_task` inserts
(`naming/redo/mod.rs:68-72`), so the task is replaced, not accumulated. `last_push` stays
push-only — a read-back is not contact with the server — so `/readyz` still reports
`lastPushSecondsAgo: null` for a resolver answering purely from a disk snapshot.

Two consequences are worth keeping in the open: a restart with Nacos unreachable now
answers from the snapshot on disk instead of serving an empty zone, and what it answers
is as old as that file. The live suite is the proof of both directions — the same
`$HOME/nacos/naming/public` entries that produced eight `timed out waiting for N
answers` failures on the pre-fix tree pass with the read-back, against those very stale
files, without clearing the directory first.

**Two §6 rows are retired by this round**: `chain rebuild fails (e.g. Nacos
unreachable) → reject whole reload` (unreachable is no longer a rebuild failure; the
remaining rebuild failures are malformed addresses, as above) and `watcher connect
fails at startup → non-fatal; retry every 30 s` (the client starts in emergency
mode and the watcher is built once, so there is nothing to retry). The two `SIGHUP`
rows are §14's.

**Test.** `e2e_unreachable_nacos_still_boots_and_subscribes_once` (port 15420,
`#[ignore]`d, the only case in the suite that needs no server): every
`server_addr` line points at `127.0.0.1:18848`, and the case waits for the SDK's
emergency `warn`, for the naming soft failure, for
`config reload listening on nacos push` exactly once, asserts no
`remote config watcher unavailable`, and asserts the process is alive and its zone
answers. Red checks, both against the live Podman `nacos-server:v2.5.4`: with both
flags off the resolver dies before the naming line can appear (the wait fails at
120 s with `Connection initialization failed after 1 attempts` as the last line);
with only the config flag off the boot survives, the config client's own health
check fails, and `remote config watcher unavailable; this process has no reload
trigger` is logged 11 s after that emergency `warn` while the push line never
appears. Green: the same run reaches `doggy-dns is ready` 11 s in, subscribes once
at +22 s, and answers NXDOMAIN inside the zone.

**Full suite, both directions.** The pre-fix tree (the flags on, no read-back) ran
`test result: FAILED. 2 passed; 8 failed` in 121.35 s, every failure a
`timed out after 60s/120s waiting for N answers` on a service whose stale entry was
sitting in `$HOME/nacos/naming/public` — `e2e-basic`, `e2e-v4only`, `e2e-known`,
`e2e-record`, `e2e-bad-push`, `e2e-reconnect`, `e2e-warm`, `e2e-reload`, the two
passers being the ones that need no live instances. With the read-back, that
directory untouched and the same snapshots in it: `test result: ok. 10 passed;
0 failed` in 65.18 s — the suite also got shorter, because answers now arrive at
subscribe time rather than on the first change.
