//! Hot config reload: one pipeline, two triggers.
//!
//! A SIGHUP re-reads the local file and a Nacos config push delivers content
//! straight from the server; both end up in
//! [`ReloadCoordinator::apply_reload`], which validates the new text with the
//! exact rules startup uses ([`doggy_dns_core::config::parse_config`]), swaps the
//! authority chain and the middleware set inside [`DoggyDnsHandler`], and
//! reports whatever it deliberately did **not** apply.
//!
//! Reload is all-or-nothing: if parsing, validation, or chain building fails,
//! the previous config stays in force and the pipeline records nothing, so a
//! corrected push is not mistaken for a repeat.
//!
//! The chain is built from [`PluginSlot`]s rather than a flat list because a
//! reload must be able to *keep* the plugins that did not change: their Nacos
//! connections, subscriptions, and — above all — warm instance caches.

use anyhow::Result;
use doggy_dns_control::live::{LiveProbe, LiveState, nacos_zone_signal};
use doggy_dns_control::state::McpState;
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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;

/// What asked for the reload; carried into every log line so a bad push and a
/// bad edit to the local file are not confused.
#[derive(Debug, Clone, Copy)]
pub enum ReloadSource {
    Sighup,
    Nacos,
}

impl std::fmt::Display for ReloadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sighup => f.write_str("sighup"),
            Self::Nacos => f.write_str("nacos"),
        }
    }
}

/// One configured plugin plus whatever was built for it.
///
/// `plugin` is the equality unit for reload reuse: when a reload's plugin at
/// the same index compares equal, the already-built handler (and the watcher
/// behind it) is carried over instead of rebuilt.
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

/// The upstream addrs used when no plugins are configured at all.
const DEFAULT_UPSTREAM: [&str; 2] = ["8.8.8.8:53", "1.1.1.1:53"];

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

/// Build the authority chain for `config`, reusing whatever is still valid
/// from `previous`.
///
/// Matching is by index and by full [`PluginConfig`] equality: an insertion in
/// the middle therefore rebuilds every plugin after it. That is the accepted
/// trade-off (see the design doc) — order is the chain's semantics, so any
/// smarter matching would have to prove it preserves it.
///
/// `previous` is only touched once every fallible step succeeded, so an error
/// leaves the caller's chain exactly as it was.
pub async fn build_chain(
    config: &DoggyDnsConfig,
    previous: &mut [PluginSlot],
) -> Result<Vec<PluginSlot>> {
    let desired = effective_plugins(config);
    let is_default_chain = config.plugins.is_empty();

    // 1. Plan: which desired entries can be carried over as they stand.
    let mut reuse: Vec<bool> = Vec::with_capacity(desired.len());
    for (i, plugin) in desired.iter().enumerate() {
        let carried = previous.get(i).is_some_and(|slot| &slot.plugin == plugin);
        reuse.push(carried);
    }

    // 2. Build what cannot be carried. Anything created here is unsubscribed
    //    and dropped if a later entry fails, so the old chain stays whole.
    let mut built: Vec<Option<PluginSlot>> = Vec::with_capacity(desired.len());
    for (i, plugin) in desired.iter().enumerate() {
        if reuse[i] {
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

    // 3. Commit: only now is `previous` consumed.
    let mut slots: Vec<PluginSlot> = Vec::with_capacity(desired.len());
    for (i, plugin) in desired.iter().enumerate() {
        if reuse[i] {
            let old = &mut previous[i];
            slots.push(PluginSlot {
                plugin: plugin.clone(),
                name: std::mem::take(&mut old.name),
                handler: old.handler.take(),
                watcher: old.watcher.take(),
                zone: old.zone.take(),
            });
        } else {
            slots.push(
                built
                    .get_mut(i)
                    .and_then(|slot| slot.take())
                    .expect("a planned fresh slot is built above"),
            );
        }
    }

    // 4. Retire: everything `previous` still holds was not carried over.
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
            let addr = plugin.server_addr.as_deref().unwrap_or("127.0.0.1:8848");
            let ns = plugin.namespace.as_deref().unwrap_or("public");
            let grp = plugin.group.as_deref().unwrap_or("DEFAULT_GROUP");
            let zone = plugin.dns_zone.as_deref().unwrap_or("nacos.local");

            let watcher = NacosServiceWatcher::new(addr, ns, grp).await?;
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
    paths.retain(|path| {
        !HOT_PATHS
            .iter()
            .any(|hot| path == hot || path.starts_with(&format!("{hot}.")))
    });
    paths
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

/// What is in force: the text a configuration came from, what it parsed to, and
/// the chain built from it.
///
/// Startup produces the three together and a reload replaces them together, so
/// they travel together — [`ReloadCoordinator::new`] takes one of these.
pub struct Applied {
    /// The raw, unexpanded text last applied — the identical-content guard and
    /// the input for the control plane's redacted snapshot.
    pub raw: String,
    pub config: DoggyDnsConfig,
    pub slots: Vec<PluginSlot>,
}

/// How a reload request ended. [`ReloadCoordinator::apply_reload`] already logs
/// the reason for each of the three, so this is for callers that need to tell
/// work from a no-op rather than read the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The new config is now in force.
    Applied,
    /// Identical to what is already in force.
    Skipped,
    /// Rejected: unreadable, invalid, or unbuildable. The old config stays.
    Rejected,
}

/// Owns the in-force chain and applies reloads to it.
///
/// One mutex guards the whole sequence: two triggers firing at once (a push
/// and a `kill -HUP`) reload one after the other, never interleaved, and the
/// query path never takes this lock.
pub struct ReloadCoordinator {
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

    /// Apply a new config, or keep the old one. Never fails the caller: a
    /// rejected reload is logged and otherwise does nothing, because a bad
    /// push must not take the resolver down.
    ///
    /// `raw` is the pushed content for [`ReloadSource::Nacos`]; `None` means
    /// re-read the startup file ([`ReloadSource::Sighup`]).
    pub async fn apply_reload(&self, source: ReloadSource, raw: Option<String>) -> ReloadOutcome {
        let raw = match raw {
            Some(raw) => raw,
            None => match std::fs::read_to_string(&self.config_path) {
                Ok(raw) => raw,
                Err(e) => {
                    tracing::error!(
                        source = %source,
                        path = %self.config_path.display(),
                        error = %e,
                        "config reload rejected: cannot read the config file"
                    );
                    return ReloadOutcome::Rejected;
                }
            },
        };

        let mut applied = self.applied.lock().await;

        if raw == applied.raw {
            tracing::info!(
                source = %source,
                "config reload skipped: content identical to the one in force"
            );
            return ReloadOutcome::Skipped;
        }

        let config = match parse_config(&raw) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!(
                    source = %source,
                    error = %e,
                    "config reload rejected: invalid configuration, keeping the one in force"
                );
                return ReloadOutcome::Rejected;
            }
        };

        let mut previous = std::mem::take(&mut applied.slots);
        let slots = match build_chain(&config, &mut previous).await {
            Ok(slots) => slots,
            Err(e) => {
                tracing::error!(
                    source = %source,
                    error = %e,
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
            state.update_runtime(&raw, &config.plugins);
        }

        let plugin_count = slots.len();
        applied.raw = raw;
        applied.config = config;
        applied.slots = slots;

        tracing::info!(
            source = %source,
            plugins = plugin_count,
            logging = applied.config.middleware.logging,
            rotate_answers = applied.config.server.rotate_answers,
            "config reload applied"
        );
        if !deferred.is_empty() {
            tracing::warn!(
                source = %source,
                paths = ?deferred,
                "these configuration changes need a restart to take effect"
            );
        }
        ReloadOutcome::Applied
    }
}
