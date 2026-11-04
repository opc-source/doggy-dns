//! Hot config reload: one pipeline, one trigger.
//!
//! A Nacos config push delivers content straight from the server into
//! [`ReloadCoordinator::apply_reload`], which validates the new text with the
//! exact rules startup uses ([`doggy_dns_core::config::parse_config`]), swaps the
//! authority chain and the middleware set inside [`DoggyDnsHandler`], and
//! reports whatever it deliberately did **not** apply. What the pipeline applies
//! is also recorded on disk next to the startup file ([`write_online_record`]),
//! so the last configuration in force survives a restart as a record — the
//! startup file itself stays the only input.
//!
//! A reload rebuilds what can be rebuilt and keeps the rest: an unchanged
//! `[[plugins]]` entry is carried over with its handler, and a nacos entry whose
//! zone or TTL moved keeps its naming connection — with the instance cache
//! already pushed into it — because a reconnect is the expensive part and a
//! rebuilt authority would have to refill from scratch.
//!
//! Reload is all-or-nothing for what it applies: if parsing, validation, or
//! chain building fails, the previous chain keeps serving and the pipeline
//! records nothing, so a corrected push is not mistaken for a repeat. The
//! baseline it records is equally strict in the other direction — only the hot
//! paths move into it, so a change that needs a restart is reported every time
//! until the restart happens.

use anyhow::Result;
use doggy_dns_control::live::{LiveProbe, LiveState, nacos_zone_signal};
use doggy_dns_control::state::McpState;
use doggy_dns_core::config::time::unix_now_secs;
use doggy_dns_core::config::{DoggyDnsConfig, PluginConfig, PluginKind, parse_config};
use doggy_dns_core::handler::DoggyDnsHandler;
use doggy_dns_core::middleware::logging::LoggingMiddleware;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::NacosServiceWatcher;
use doggy_dns_plugin::Middleware;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use doggy_dns_plugin::builtin::native::NativeAuthority;
use hickory_server::zone_handler::ZoneHandler;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;

/// One configured plugin plus whatever was built for it.
///
/// `plugin` is what a reload matches on: unchanged at the same index, the whole
/// slot carries over with its handler; changed in zone or TTL only, the `watcher`
/// is handed to the new slot and only the authority above it is rebuilt, so the
/// warm instance cache comes along.
pub struct PluginSlot {
    plugin: PluginConfig,
    /// Display name recorded in the authority chain.
    name: String,
    /// `None` for a disabled plugin, which contributes no handler.
    handler: Option<Arc<dyn ZoneHandler>>,
    /// The watcher backing a `nacos` handler; it must outlive the slot's use
    /// and be unsubscribed when the slot is retired.
    watcher: Option<NacosServiceWatcher>,
    /// The DNS zone served by a `nacos` plugin, for the control plane's live
    /// cache signals.
    zone: Option<String>,
}

impl PluginSlot {
    fn nacos_watcher(&self) -> Option<(&str, &NacosServiceWatcher)> {
        let zone = self.zone.as_deref()?;
        self.watcher.as_ref().map(|w| (zone, w))
    }

    /// The config entry this slot was built from (or reused for).
    pub fn plugin(&self) -> &PluginConfig {
        &self.plugin
    }

    /// The name recorded in the authority chain.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The built handler, if the plugin is enabled. Reuse across a reload
    /// keeps the same `Arc`, so pointer identity is the witness that nothing
    /// was rebuilt.
    pub fn handler(&self) -> Option<&Arc<dyn ZoneHandler>> {
        self.handler.as_ref()
    }
}

/// The control plane's live Nacos signals for the zones these slots own.
///
/// Each probe reads the watcher's own cache and push timestamp, so it must be
/// rebuilt from the slots currently in force: a retired watcher's numbers
/// belong to nothing.
pub fn nacos_zone_signals(slots: &[PluginSlot]) -> Vec<LiveProbe> {
    slots
        .iter()
        .filter_map(|slot| {
            let (zone, watcher) = slot.nacos_watcher()?;
            let cache = watcher.cache();
            let last_push = watcher.last_push();
            Some(nacos_zone_signal(
                zone,
                move || cache.len() as u64,
                move || last_push.load(Ordering::Relaxed),
            ))
        })
        .collect()
}

/// The values a nacos plugin falls back to when the config leaves them unset.
const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:8848";
const DEFAULT_NAMESPACE: &str = "public";
const DEFAULT_GROUP: &str = "DEFAULT_GROUP";
const DEFAULT_ZONE: &str = "nacos.local";

/// The upstream addrs used when no plugins are configured at all.
const DEFAULT_UPSTREAM: [&str; 2] = ["8.8.8.8:53", "1.1.1.1:53"];

/// What identifies one naming connection.
///
/// Zone and TTL are deliberately absent: they shape the answers built on top of
/// a cache, not the connection feeding it. Changing either one therefore keeps
/// the connection — and every instance already pushed into its cache.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NacosIdentity {
    server_addr: String,
    namespace: String,
    group: String,
}

/// The connection coordinates of a nacos plugin, defaults applied.
fn nacos_identity(plugin: &PluginConfig) -> NacosIdentity {
    NacosIdentity {
        server_addr: plugin
            .server_addr
            .clone()
            .unwrap_or_else(|| DEFAULT_SERVER_ADDR.to_string()),
        namespace: plugin
            .namespace
            .clone()
            .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
        group: plugin
            .group
            .clone()
            .unwrap_or_else(|| DEFAULT_GROUP.to_string()),
    }
}

/// The [`PluginConfig`] that describes the implicit default forward plugin.
///
/// A zero-config start is a real plugin chain, so reload treats it as one:
/// this stands in for the absent `[[plugins]]` list, which lets the reuse rule
/// below carry the default chain across reloads instead of rebuilding it. The
/// numbers are the ones [`build_default_slot`] uses, which are deliberately not
/// the serde defaults — that is what keeps this entry from ever comparing equal
/// to an explicit `[[plugins]]` entry.
fn synthetic_default_forward() -> PluginConfig {
    PluginConfig {
        kind: PluginKind::Forward,
        enabled: true,
        server_addr: None,
        namespace: None,
        group: None,
        dns_zone: None,
        ttl: 6,
        cache_size: 1024,
        min_ttl: 30,
        max_ttl: 600,
        upstream: None,
    }
}

/// The plugin list the chain is built from: `[[plugins]]`, or the implicit
/// default forward when there is none.
fn effective_plugins(config: &DoggyDnsConfig) -> Vec<PluginConfig> {
    if config.plugins.is_empty() {
        vec![synthetic_default_forward()]
    } else {
        config.plugins.clone()
    }
}

/// What a reload will do with one desired plugin, decided before anything is
/// built so that the fallible part and the consuming part stay separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotPlan {
    /// Carry the previous slot at this index: unchanged config, same handler.
    Carry(usize),
    /// Keep the naming connection of the previous slot at this index and build
    /// a fresh authority above it: only the zone or the TTL moved.
    ReuseConnection(usize),
    /// Build from scratch — a new connection, a new forwarder, an empty cache.
    Build,
}

/// The share of a previous slot a reuse plan can see.
///
/// The plan is pure: it decides from configuration alone, so the rules are
/// testable without a naming service behind them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousEntry {
    pub plugin: PluginConfig,
    /// Whether the slot still owns a live naming connection.
    pub has_connection: bool,
}

impl PreviousEntry {
    fn of(slot: &PluginSlot) -> Self {
        Self {
            plugin: slot.plugin.clone(),
            has_connection: slot.watcher.is_some(),
        }
    }
}

/// Plan one reload: which desired entries are carried, which keep a connection,
/// which have to be built.
///
/// Exact matches are index-pinned, because order is the chain's semantics; a
/// nacos connection is matched by identity anywhere in the previous chain, so
/// reordering two zones on one server keeps both connections. Matching consumes:
/// a previous entry backs at most one desired entry.
pub fn plan_slots(desired: &[PluginConfig], previous: &[PreviousEntry]) -> Vec<SlotPlan> {
    let mut taken = vec![false; previous.len()];

    // 1. Unchanged entries carry over as they stand.
    let mut plans: Vec<SlotPlan> = desired
        .iter()
        .enumerate()
        .map(|(index, plugin)| {
            if previous
                .get(index)
                .is_some_and(|entry| &entry.plugin == plugin)
            {
                taken[index] = true;
                SlotPlan::Carry(index)
            } else {
                SlotPlan::Build
            }
        })
        .collect();

    // 2. What is left may still want an existing connection.
    for (index, plugin) in desired.iter().enumerate() {
        if plans[index] != SlotPlan::Build || !plugin.enabled || plugin.kind != PluginKind::Nacos {
            continue;
        }
        let want = nacos_identity(plugin);
        // The first free connection that matches, not the first free connection:
        // two plugins on one server reorder, and a `find` followed by a `filter`
        // would let the second entry's connection go unclaimed.
        let free = previous
            .iter()
            .enumerate()
            .find(|(other, entry)| {
                !taken[*other] && entry.has_connection && nacos_identity(&entry.plugin) == want
            })
            .map(|(other, _)| other);
        if let Some(other) = free {
            taken[other] = true;
            plans[index] = SlotPlan::ReuseConnection(other);
        }
    }

    plans
}

/// Build the authority chain for `config`, reusing whatever is still valid
/// from `previous`.
///
/// An entry whose [`PluginConfig`] is unchanged is carried over whole, handler
/// included. One whose zone or TTL changed keeps its naming connection and only
/// rebuilds the authority sitting on top of the shared cache, which is what
/// makes the commonest operational change — a TTL tweak — cost nothing but a
/// struct. Anything else is rebuilt, and whatever `previous` still holds at the
/// end is unsubscribed, so a reload never leaves a subscription behind.
///
/// `previous` is only touched once every fallible step succeeded, so an error
/// leaves the caller's chain exactly as it was.
pub async fn build_chain(
    config: &DoggyDnsConfig,
    previous: &mut [PluginSlot],
) -> Result<Vec<PluginSlot>> {
    let desired = effective_plugins(config);
    let is_default_chain = config.plugins.is_empty();
    let entries: Vec<PreviousEntry> = previous.iter().map(PreviousEntry::of).collect();
    let plans = plan_slots(&desired, &entries);

    // 1. Build what cannot be carried. Anything created here is unsubscribed
    //    and dropped if a later entry fails, so the old chain stays whole.
    let mut built: Vec<Option<PluginSlot>> = Vec::with_capacity(desired.len());
    for (i, plugin) in desired.iter().enumerate() {
        if plans[i] != SlotPlan::Build {
            built.push(None);
            continue;
        }
        let slot = if is_default_chain {
            build_default_slot().await
        } else {
            build_slot(plugin).await
        };
        match slot {
            Ok(slot) => {
                if slot.handler.is_some() {
                    tracing::info!(plugin = %slot.name, "loaded plugin");
                } else {
                    tracing::info!(index = i, kind = ?plugin.kind, "skipping disabled plugin");
                }
                built.push(Some(slot));
            }
            Err(e) => {
                for partial in built.iter().flatten() {
                    if let Some(watcher) = &partial.watcher {
                        watcher.unsubscribe_all().await;
                    }
                }
                return Err(e);
            }
        }
    }

    // 2. Commit: only now is `previous` consumed. Reuse is infallible — a
    //    connection is either there or it is not, and the plan said it is.
    let mut slots: Vec<PluginSlot> = Vec::with_capacity(desired.len());
    for (i, plugin) in desired.iter().enumerate() {
        let slot = match plans[i] {
            SlotPlan::Carry(old) => {
                let previous = &mut previous[old];
                PluginSlot {
                    plugin: plugin.clone(),
                    name: std::mem::take(&mut previous.name),
                    handler: previous.handler.take(),
                    watcher: previous.watcher.take(),
                    zone: previous.zone.take(),
                }
            }
            SlotPlan::ReuseConnection(old) => {
                let previous = &mut previous[old];
                // `plan_slots` only plans a reuse for a slot that owns a
                // watcher, and step 1 never touched this one.
                let watcher = previous
                    .watcher
                    .take()
                    .expect("a planned reuse owns a watcher");
                let zone = plugin
                    .dns_zone
                    .as_deref()
                    .unwrap_or(DEFAULT_ZONE)
                    .to_string();
                let authority = NacosAuthority::new(watcher.cache(), &zone, plugin.ttl);
                let name = format!("nacos[{zone}]");
                tracing::info!(
                    plugin = %name,
                    addr = %nacos_identity(plugin).server_addr,
                    "nacos connection reused"
                );
                // The old slot's handler was built on the same cache but with
                // the previous zone/TTL; drop it, keep the connection.
                previous.handler = None;
                previous.name.clear();
                previous.zone = None;
                PluginSlot {
                    plugin: plugin.clone(),
                    name,
                    handler: Some(Arc::new(authority)),
                    watcher: Some(watcher),
                    zone: Some(zone),
                }
            }
            SlotPlan::Build => built[i]
                .take()
                .expect("a planned fresh slot is built above"),
        };
        slots.push(slot);
    }

    // 3. Retire: everything `previous` still holds was not carried over.
    for old in previous.iter_mut() {
        if let Some(watcher) = old.watcher.take() {
            watcher.unsubscribe_all().await;
        }
    }

    Ok(slots)
}

/// The implicit chain of a zero-config start.
async fn build_default_slot() -> Result<PluginSlot> {
    tracing::info!("no plugins configured, defaulting to forward");
    let forward = ForwardAuthority::new(
        DEFAULT_UPSTREAM.map(String::from).to_vec(),
        1024,
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await?;
    Ok(PluginSlot {
        plugin: synthetic_default_forward(),
        name: "forward[default]".to_string(),
        handler: Some(Arc::new(forward)),
        watcher: None,
        zone: None,
    })
}

/// Build the handler (and watcher) for one configured plugin.
async fn build_slot(plugin: &PluginConfig) -> Result<PluginSlot> {
    if !plugin.enabled {
        return Ok(PluginSlot {
            plugin: plugin.clone(),
            name: String::new(),
            handler: None,
            watcher: None,
            zone: None,
        });
    }

    match plugin.kind {
        PluginKind::Nacos => {
            let identity = nacos_identity(plugin);
            let zone = plugin.dns_zone.as_deref().unwrap_or(DEFAULT_ZONE);

            let watcher = NacosServiceWatcher::new(
                &identity.server_addr,
                &identity.namespace,
                &identity.group,
            )
            .await?;
            watcher.subscribe_all().await;
            let cache = watcher.cache();

            let authority = NacosAuthority::new(cache, zone, plugin.ttl);
            Ok(PluginSlot {
                plugin: plugin.clone(),
                name: format!("nacos[{zone}]"),
                handler: Some(Arc::new(authority)),
                watcher: Some(watcher),
                zone: Some(zone.to_string()),
            })
        }
        PluginKind::Native => {
            let authority = NativeAuthority::new(
                plugin.cache_size as u64,
                Duration::from_secs(plugin.min_ttl as u64),
                Duration::from_secs(plugin.max_ttl as u64),
            )
            .await?;
            Ok(PluginSlot {
                plugin: plugin.clone(),
                name: "native".to_string(),
                handler: Some(Arc::new(authority)),
                watcher: None,
                zone: None,
            })
        }
        PluginKind::Forward => {
            let addrs = plugin
                .upstream
                .clone()
                .unwrap_or_else(|| DEFAULT_UPSTREAM.iter().map(|s| s.to_string()).collect());
            let authority = ForwardAuthority::new(
                addrs.clone(),
                plugin.cache_size as u64,
                Duration::from_secs(plugin.min_ttl as u64),
                Duration::from_secs(plugin.max_ttl as u64),
            )
            .await?;
            Ok(PluginSlot {
                plugin: plugin.clone(),
                name: format!("forward[{}]", addrs.join(",")),
                handler: Some(Arc::new(authority)),
                watcher: None,
                zone: None,
            })
        }
    }
}

/// Assemble the chain the handler resolves through, in configured order.
pub fn authority_chain_from(slots: &[PluginSlot]) -> AuthorityChain {
    let handlers: Vec<(String, Arc<dyn ZoneHandler>)> = slots
        .iter()
        .filter_map(|slot| {
            slot.handler
                .as_ref()
                .map(|handler| (slot.name.clone(), Arc::clone(handler)))
        })
        .collect();
    AuthorityChain::new(handlers)
}

/// The middleware set for `logging_enabled`: the logging middleware first (as
/// at startup) followed by the middlewares that outlive any reload — metrics
/// and XDP, which the coordinator captured at startup.
pub fn middlewares_for(
    logging_enabled: bool,
    base: &[Arc<dyn Middleware>],
) -> Vec<Arc<dyn Middleware>> {
    let mut middlewares: Vec<Arc<dyn Middleware>> = Vec::with_capacity(base.len() + 1);
    if logging_enabled {
        middlewares.push(Arc::new(LoggingMiddleware));
    }
    middlewares.extend_from_slice(base);
    middlewares
}

/// Config paths a reload does **not** apply.
const HOT_PATHS: &[&str] = &["plugins", "middleware.logging", "server.rotate_answers"];

/// Paths whose values differ between the running and the incoming config but
/// can only take effect after a restart.
///
/// The comparison is a walk over the two serialised configs, so a new config
/// section is reported as restart-required without anyone remembering to add
/// it to a list; only the three hot paths above are filtered out.
///
/// Reporting only: a config that cannot be serialised to TOML yields an empty
/// list and a warn, never a failed reload.
pub fn restart_required(old: &DoggyDnsConfig, new: &DoggyDnsConfig) -> Vec<String> {
    let (Ok(old), Ok(new)) = (toml::Value::try_from(old), toml::Value::try_from(new)) else {
        tracing::warn!(
            "cannot compare the two configurations; restart-required paths not reported"
        );
        return Vec::new();
    };
    let mut paths = Vec::new();
    collect_diffs("", &old, &new, &mut paths);
    paths.retain(|path| !is_hot_path(path));
    paths
}

/// Whether `path` names the hot subset or something under it.
fn is_hot_path(path: &str) -> bool {
    !path.is_empty()
        && HOT_PATHS
            .iter()
            .any(|hot| path == *hot || path.starts_with(&format!("{}.", *hot)))
}

fn collect_diffs(prefix: &str, old: &toml::Value, new: &toml::Value, out: &mut Vec<String>) {
    if let (toml::Value::Table(a), toml::Value::Table(b)) = (old, new) {
        let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        for key in keys {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match (a.get(key), b.get(key)) {
                (Some(x), Some(y)) => collect_diffs(&path, x, y, out),
                _ => out.push(path),
            }
        }
    } else if old != new {
        out.push(prefix.to_string());
    }
}

/// `new` with every non-hot path replaced by the value still in force.
///
/// This is what a reload records as the baseline: the hot subset moved, the rest
/// of the process is unchanged, and the next comparison has to say so again.
/// Absorbing a restart-required field into the baseline instead would let the
/// first reload after an edit silence the warning while the socket is still
/// bound to the old value.
///
/// Reporting only, like [`restart_required`]: a config that cannot make the
/// round trip through TOML is taken as in force, with a warn.
fn adopt_hot_paths_only(old: &DoggyDnsConfig, new: &DoggyDnsConfig) -> DoggyDnsConfig {
    let merged = match (toml::Value::try_from(old), toml::Value::try_from(new)) {
        (Ok(old_value), Ok(new_value)) => keep_hot_only("", &old_value, &new_value),
        _ => {
            tracing::warn!(
                "cannot compare configurations as TOML; the incoming config is taken as in force"
            );
            return new.clone();
        }
    };
    match merged.try_into::<DoggyDnsConfig>() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                %error,
                "merged configuration does not deserialise; the incoming config is taken as in force"
            );
            new.clone()
        }
    }
}

/// The tables of `new` with everything outside [`HOT_PATHS`] taken from `old`.
///
/// Shape matters as much as value here. A section `new` adds that the process
/// never opened stays out of the baseline — an added `[control]` or
/// `[remote_config]` is still a restart-required difference — and a hot path the
/// incoming config drops leaves the baseline too, because the chain the reload
/// built really is the one without it.
fn keep_hot_only(prefix: &str, old: &toml::Value, new: &toml::Value) -> toml::Value {
    if let (toml::Value::Table(old), toml::Value::Table(new)) = (old, new) {
        let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let mut merged = toml::Table::new();
        for key in keys {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            if is_hot_path(&path) {
                // Adopted as a whole, "the incoming config has none" included.
                if let Some(value) = new.get(key) {
                    merged.insert(key.clone(), value.clone());
                }
                continue;
            }
            match (old.get(key), new.get(key)) {
                (Some(old_value), Some(new_value)) => {
                    merged.insert(key.clone(), keep_hot_only(&path, old_value, new_value));
                }
                // In force and no longer asked for: the process still runs it, so
                // the next comparison has to keep reporting the difference.
                (Some(old_value), None) => {
                    merged.insert(key.clone(), old_value.clone());
                }
                // Asked for and never applied: not in force, so it stays out.
                (None, Some(_)) | (None, None) => {}
            }
        }
        return toml::Value::Table(merged);
    }
    old.clone()
}

/// What is in force: the text a configuration came from, what the running
/// process actually applies of it, the chain built from it, and how many times
/// it has been reloaded.
///
/// Startup produces these together and a reload replaces them together, so they
/// travel together — [`ReloadCoordinator::new`] takes one of these.
///
/// `config` is deliberately *not* the whole of `raw`: a reload applies the hot
/// paths and leaves the rest at the values the process is really running, so the
/// restart-required warning keeps naming the same fields until the restart
/// happens ([`adopt_hot_paths_only`]).
pub struct Applied {
    /// The raw, unexpanded text last applied — the identical-content guard and
    /// the input for the control plane's redacted snapshot.
    pub raw: String,
    pub config: DoggyDnsConfig,
    pub slots: Vec<PluginSlot>,
    /// Reloads applied so far; 0 is the start-up configuration.
    pub generation: u64,
}

/// How a reload request ended. [`ReloadCoordinator::apply_reload`] already logs
/// the reason for each of the three, so this is for callers that need to tell
/// work from a no-op rather than read the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The new config is now in force, at this generation.
    Applied { generation: u64 },
    /// Identical to what is already in force.
    Skipped,
    /// Rejected: invalid, or unbuildable. The old config stays.
    Rejected,
}

/// How long a burst of pushes is folded into one reload.
///
/// The window exists for the remote trigger: an edit that changes several lines
/// arrives as several pushes, and each reload would rebuild the chain again.
/// A burst therefore costs one reload — the newest push, because it carries the
/// newest intent. Folding stops as soon as the channel is quiet for the window,
/// so a steady stream of pushes still gets served, one reload per window.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// Apply pushed configurations as they arrive, one at a time, coalescing bursts.
///
/// The trigger hands its content over and returns, which keeps a config
/// callback from awaiting a rebuild it cannot cancel; the channel closing (the
/// last sender dropped, i.e. shutdown) ends the task.
pub async fn run_reload_loop(
    coordinator: Arc<ReloadCoordinator>,
    mut pushes: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    while let Some(first) = pushes.recv().await {
        let mut newest = first;
        while let Ok(Some(push)) = tokio::time::timeout(RELOAD_DEBOUNCE, pushes.recv()).await {
            newest = push;
        }
        coordinator.apply_reload(&newest).await;
    }
}

/// Owns the in-force chain and applies reloads to it.
///
/// One mutex guards the whole sequence: pushes that arrive while a reload is
/// running are applied one after the other, never interleaved, and the query
/// path never takes this lock.
pub struct ReloadCoordinator {
    /// The startup file; its sibling holds the record of what is in force
    /// ([`online_record_path`]).
    config_path: PathBuf,
    handler: Arc<DoggyDnsHandler>,
    /// Everything but the logging middleware, captured at startup; a reload
    /// composes the new set from this plus (optionally) logging.
    base_middlewares: Vec<Arc<dyn Middleware>>,
    control_state: Option<Arc<McpState>>,
    /// The control plane's live signals: a reload changes which Nacos zones
    /// exist, so their probes have to be rebuilt from the surviving slots.
    live: Arc<LiveState>,
    applied: Mutex<Applied>,
}

impl ReloadCoordinator {
    /// Adopt the chain built at startup.
    pub fn new(
        config_path: PathBuf,
        applied: Applied,
        handler: Arc<DoggyDnsHandler>,
        base_middlewares: Vec<Arc<dyn Middleware>>,
        control_state: Option<Arc<McpState>>,
        live: Arc<LiveState>,
    ) -> Self {
        Self {
            config_path,
            handler,
            base_middlewares,
            control_state,
            live,
            applied: Mutex::new(applied),
        }
    }

    /// The config currently in force.
    pub async fn applied_config(&self) -> DoggyDnsConfig {
        self.applied.lock().await.config.clone()
    }

    /// How many reloads have been applied; 0 means the start-up config is
    /// still in force. A rejected or skipped request does not move it.
    pub async fn generation(&self) -> u64 {
        self.applied.lock().await.generation
    }

    /// Apply a pushed configuration, or keep the one in force. Never fails the
    /// caller: a rejected push is logged and otherwise does nothing, because a
    /// bad push must not take the resolver down.
    pub async fn apply_reload(&self, raw: &str) -> ReloadOutcome {
        let mut applied = self.applied.lock().await;

        if raw == applied.raw {
            tracing::info!("config reload skipped: content identical to the one in force");
            return ReloadOutcome::Skipped;
        }

        let config = match parse_config(raw) {
            Ok(config) => config,
            Err(error) => {
                tracing::error!(
                    %error,
                    "config reload rejected: invalid configuration, keeping the one in force"
                );
                return ReloadOutcome::Rejected;
            }
        };

        let mut previous = std::mem::take(&mut applied.slots);
        let slots = match build_chain(&config, &mut previous).await {
            Ok(slots) => slots,
            Err(error) => {
                tracing::error!(
                    %error,
                    "config reload rejected: authority chain could not be built, keeping the one in force"
                );
                // Nothing was consumed on the failure path, but put the chain
                // back exactly where it was.
                applied.slots = previous;
                return ReloadOutcome::Rejected;
            }
        };

        let deferred = restart_required(&applied.config, &config);
        let middlewares = middlewares_for(config.middleware.logging, &self.base_middlewares);

        self.handler
            .swap_runtime(middlewares, Arc::new(authority_chain_from(&slots)));
        self.handler.set_rotation(config.server.rotate_answers);
        self.live.replace_nacos_zones(nacos_zone_signals(&slots));
        if let Some(state) = &self.control_state {
            state.update_runtime(raw, &config.plugins);
        }

        let plugin_count = slots.len();
        applied.raw = raw.to_string();
        applied.config = adopt_hot_paths_only(&applied.config, &config);
        applied.slots = slots;
        applied.generation += 1;
        let generation = applied.generation;

        tracing::info!(
            generation,
            plugins = plugin_count,
            logging = applied.config.middleware.logging,
            rotate_answers = applied.config.server.rotate_answers,
            "config reload applied"
        );
        if !deferred.is_empty() {
            tracing::warn!(
                generation,
                paths = ?deferred,
                "these configuration changes need a restart to take effect"
            );
        }
        write_online_record(&self.config_path, raw);
        ReloadOutcome::Applied { generation }
    }
}

/// How many timestamped copies of the in-force record a machine keeps.
const ONLINE_BACKUPS_KEPT: usize = 10;

/// The file that records the configuration currently in force: a sibling of the
/// startup file, named `<stem>.online.toml`.
///
/// A record, not an input: startup reads `config_path` and only `config_path`,
/// so a bad remote configuration can never become the thing the process boots
/// into.
pub fn online_record_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    config_path.with_file_name(format!("{stem}.online.toml"))
}

/// Record `raw` as the configuration in force, moving the version it replaces
/// to a timestamped backup first.
///
/// The text written is what was pushed, restart-required fields and all, so the
/// next boot applies the whole of it; `${VAR}` references stay unexpanded
/// because that is the text the pipeline holds. Three renames rather than one
/// write, so an interrupted run leaves either the old record or the new one and
/// never a half-written file: the text lands in a `.tmp` sibling and is synced,
/// the previous record moves aside under `<record>.bak.<epoch secs>`, and only
/// then does the temp file take its place.
///
/// Failures are reported and otherwise dropped. The reload this records already
/// happened, and a read-only directory is not a reason to undo it.
pub fn write_online_record(config_path: &Path, raw: &str) {
    let record = online_record_path(config_path);
    let temp = appended(&record, ".tmp");

    if let Err(error) = write_pending(&temp, raw, &record) {
        tracing::warn!(%error, path = %record.display(), "config online record not written");
        return;
    }

    let backup = next_backup_path(&record);
    match std::fs::rename(&record, &backup) {
        Ok(()) => {}
        // The first record has no previous version to keep.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            tracing::warn!(%error, path = %record.display(), "config online record not written");
            return;
        }
    }

    if let Err(error) = std::fs::rename(&temp, &record) {
        // Put back what was there when this started, so the file on disk stays
        // the one the operator last read.
        let _ = std::fs::rename(&backup, &record);
        let _ = std::fs::remove_file(&temp);
        tracing::error!(%error, path = %record.display(), "config online record could not be put in place");
        return;
    }

    prune_online_backups(&record);
}

/// `path` with `suffix` appended to its file name.
fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// The next backup name: `<record>.bak.<epoch secs>`, moved past any name
/// already taken so two reloads inside one second keep two generations.
fn next_backup_path(record: &Path) -> PathBuf {
    let mut stamp = unix_now_secs();
    loop {
        let candidate = appended(record, &format!(".bak.{stamp}"));
        if !candidate.exists() {
            return candidate;
        }
        stamp += 1;
    }
}

/// Get `raw` onto disk at `temp`, wearing whatever permissions the record had.
fn write_pending(temp: &Path, raw: &str, record: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::create(temp)?;
    if let Ok(metadata) = std::fs::metadata(record) {
        // A platform that refuses the copy is not worth losing the record over.
        let _ = file.set_permissions(metadata.permissions());
    }
    file.write_all(raw.as_bytes())?;
    file.flush()?;
    file.sync_all()
}

/// Drop all but the newest [`ONLINE_BACKUPS_KEPT`] backups of `record`.
fn prune_online_backups(record: &Path) {
    let (Some(prefix), Some(dir)) = (
        record
            .file_name()
            .map(|name| format!("{}.bak.", name.to_string_lossy())),
        record.parent(),
    ) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<(i64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let stamp: i64 = entry
                .file_name()
                .to_string_lossy()
                .strip_prefix(&prefix)?
                .parse()
                .ok()?;
            Some((stamp, entry.path()))
        })
        .collect();
    if backups.len() <= ONLINE_BACKUPS_KEPT {
        return;
    }
    backups.sort_unstable_by(|(_, a), (_, b)| b.cmp(a));
    for (_, path) in backups.iter().skip(ONLINE_BACKUPS_KEPT) {
        if let Err(error) = std::fs::remove_file(path) {
            tracing::warn!(%error, path = %path.display(), "old config online backup could not be removed");
        }
    }
}
