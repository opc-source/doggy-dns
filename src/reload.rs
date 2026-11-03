//! Apply a configuration change to the running process.
//!
//! Two sources ask for it: `SIGHUP` (re-read the config file from disk) and
//! the Nacos remote-config watcher (hand over new TOML content). Both end up
//! in [`ReloadOrchestrator::apply`], which parses and validates the candidate,
//! refuses it if anything outside the reloadable query path changed, then
//! rebuilds the authority chain and swaps it into the live handler.
//!
//! The chain rebuild reuses pooled Nacos connections (`doggy_dns_nacos::pool`)
//! so a reload that only touches TTLs or plugins does not reconnect. The chain
//! and the answer-rotation flag go over in one store, so no query can pair the
//! new chain with the old flag; a query in flight keeps answering from the path
//! it started on. The `middleware.logging` flag cannot join that store — it is
//! read by the middleware chain, which is never rebuilt — so it is a separate
//! atomic write, and a query may see a reload apply to one and not the other.
//!
//! A unit-testable seam exists here because the binary crate also has a lib
//! target (`src/lib.rs`); the swap itself is exercised end-to-end by
//! `tests/nacos_e2e_test.rs`.

use anyhow::{Context, Result};
use doggy_dns_core::config::{
    DoggyDnsConfig, PluginKind, load_config, parse_config, reload_conflicts,
};
use doggy_dns_core::handler::ReloadHandle;
use doggy_dns_core::middleware::logging::LoggingToggle;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::config_watcher::NacosConfigWatcher;
use doggy_dns_nacos::pool::{NacosTarget, PooledWatcher, ReusePlan, plan_reuse};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use doggy_dns_plugin::builtin::native::NativeAuthority;
use hickory_server::zone_handler::ZoneHandler;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// Default upstreams when the config asks for no plugins at all.
const DEFAULT_UPSTREAMS: [&str; 2] = ["8.8.8.8:53", "1.1.1.1:53"];

/// A request to reload, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadRequest {
    /// Re-read the config file the process started with.
    Sighup,
    /// Content pushed by the remote config service.
    Nacos { content: String },
}

impl ReloadRequest {
    fn source(&self) -> &'static str {
        match self {
            Self::Sighup => "sighup",
            Self::Nacos { .. } => "nacos",
        }
    }
}

/// What a reload attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The new query path is live.
    Applied {
        /// Reload generation, counting from 1.
        generation: u64,
        /// Authorities the rebuilt chain holds.
        authorities: usize,
    },
    /// The candidate changed something a running process cannot change;
    /// nothing was applied.
    Rejected {
        /// Config paths that differ, e.g. `server.port`.
        conflicts: Vec<String>,
    },
    /// The candidate is identical to the config already in force; nothing was
    /// rebuilt. A multi-line edit delivered as several pushes often ends with
    /// one of these once the debounce window has already applied it.
    Skipped,
    /// The candidate was unreadable, unparseable, invalid, or the new chain
    /// could not be built; the previous path stays live.
    Failed { error: String },
}

/// Rebuild and swap the query path, single-flighted.
///
/// The mutex serialises reloads rather than racing them: a burst of `SIGHUP`s
/// or config pushes is applied one at a time, each against the config the
/// previous one installed.
pub struct ReloadOrchestrator {
    config_path: String,
    reload: ReloadHandle,
    logging: LoggingToggle,
    state: Mutex<ReloadState>,
}

struct ReloadState {
    current: DoggyDnsConfig,
    pool: Vec<PooledWatcher>,
    generation: u64,
}

impl ReloadOrchestrator {
    pub fn new(
        config_path: String,
        config: DoggyDnsConfig,
        pool: Vec<PooledWatcher>,
        reload: ReloadHandle,
        logging: LoggingToggle,
    ) -> Self {
        Self {
            config_path,
            reload,
            logging,
            state: Mutex::new(ReloadState {
                current: config,
                pool,
                generation: 0,
            }),
        }
    }

    /// Generation of the config currently in force; 0 means the start-up one.
    pub async fn generation(&self) -> u64 {
        self.state.lock().await.generation
    }

    /// The config currently in force.
    pub async fn current_config(&self) -> DoggyDnsConfig {
        self.state.lock().await.current.clone()
    }

    /// Apply one reload request.
    ///
    /// Every failure path leaves the running process exactly as it was: the
    /// old chain keeps answering, the old config stays authoritative, and the
    /// generation does not move.
    pub async fn apply(&self, request: ReloadRequest) -> ReloadOutcome {
        let source = request.source();
        let mut state = self.state.lock().await;

        let candidate = match self::read_candidate(&self.config_path, request) {
            Ok(config) => config,
            Err(error) => {
                tracing::error!(source, generation = state.generation, %error, "config reload failed to read");
                return ReloadOutcome::Failed {
                    error: error.to_string(),
                };
            }
        };

        let conflicts = reload_conflicts(&state.current, &candidate);
        if !conflicts.is_empty() {
            tracing::error!(
                source,
                generation = state.generation,
                conflicts = %conflicts.join(", "),
                "config reload rejected: restart required"
            );
            return ReloadOutcome::Rejected { conflicts };
        }

        if candidate == state.current {
            tracing::info!(
                source,
                generation = state.generation,
                "config reload skipped: candidate identical to the config in force"
            );
            return ReloadOutcome::Skipped;
        }

        let (chain, pool) = match build_chain(&candidate, &state.pool).await {
            Ok(built) => built,
            Err(error) => {
                tracing::error!(source, generation = state.generation, %error, "config reload failed to build authority chain");
                return ReloadOutcome::Failed {
                    error: error.to_string(),
                };
            }
        };
        let retired: Vec<PooledWatcher> = state
            .pool
            .iter()
            .filter(|old| !pool.iter().any(|new| new.same_watcher(old)))
            .cloned()
            .collect();

        let authorities = chain.len();
        self.reload
            .swap_query_path(chain, candidate.server.rotate_answers);
        self.logging.set(candidate.middleware.logging);

        state.current = candidate;
        state.pool = pool;
        state.generation += 1;
        let generation = state.generation;
        let rotate_answers = state.current.server.rotate_answers;
        let logging = state.current.middleware.logging;
        drop(state);

        // The new chain is live; a watcher it does not use must stop filling a
        // cache nothing reads. This cannot fail the reload.
        for watcher in &retired {
            watcher.retire().await;
        }

        tracing::info!(
            source,
            generation,
            authorities,
            rotate_answers,
            logging,
            retired = retired.len(),
            "config reload applied"
        );
        ReloadOutcome::Applied {
            generation,
            authorities,
        }
    }
}

/// Read the candidate config from wherever the request carries it.
fn read_candidate(path: &str, request: ReloadRequest) -> Result<DoggyDnsConfig> {
    match request {
        ReloadRequest::Sighup => load_config(path)
            .map(|(config, _)| config)
            .with_context(|| format!("failed to load config from {path}")),
        ReloadRequest::Nacos { content } => parse_config(&content)
            .context("failed to parse config pushed by the remote config service"),
    }
}

/// Build the authority chain from `[[plugins]]`, reusing pooled watchers.
///
/// The staged pool is returned instead of being written back into `previous`:
/// every fallible step happens before the caller commits, so a failed rebuild
/// leaves the live pool and every watcher's zone exactly as they were. Watchers
/// opened on the way are unsubscribed if a later step fails. A watcher the new
/// chain does not use is absent from the returned pool; the caller retires it
/// after committing, so a reload never leaves a subscription behind.
pub async fn build_chain(
    config: &DoggyDnsConfig,
    previous: &[PooledWatcher],
) -> Result<(AuthorityChain, Vec<PooledWatcher>)> {
    if config.plugins.is_empty() {
        tracing::info!("no plugins configured, defaulting to forward");
        let forward = default_forward().await?;
        let handlers: Vec<(String, Arc<dyn ZoneHandler>)> = vec![(
            "forward[default]".to_string(),
            Arc::new(forward) as Arc<dyn ZoneHandler>,
        )];
        return Ok((AuthorityChain::new(handlers), Vec::new()));
    }

    let targets: Vec<NacosTarget> = config
        .plugins
        .iter()
        .filter(|plugin| plugin.enabled && plugin.kind == PluginKind::Nacos)
        .map(NacosTarget::from_plugin)
        .collect();
    let pooled_identities: Vec<_> = previous
        .iter()
        .map(|watcher| watcher.identity.clone())
        .collect();
    let plans = plan_reuse(&targets, &pooled_identities);

    let mut staged: Vec<PooledWatcher> = Vec::with_capacity(targets.len());
    let mut opened: Vec<usize> = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        match plans[index] {
            ReusePlan::Existing(pool_index) => {
                let mut watcher = previous
                    .get(pool_index)
                    .with_context(|| format!("pooled watcher index {pool_index} vanished"))?
                    .clone();
                watcher.zone.clone_from(&target.zone);
                staged.push(watcher);
            }
            ReusePlan::Open => match PooledWatcher::open(target).await {
                Ok(watcher) => {
                    opened.push(staged.len());
                    staged.push(watcher);
                }
                Err(error) => {
                    retire_opened(&staged, &opened).await;
                    return Err(error);
                }
            },
        }
    }

    let handlers = match assemble_handlers(config, &targets, &plans, &staged).await {
        Ok(handlers) => handlers,
        Err(error) => {
            retire_opened(&staged, &opened).await;
            return Err(error);
        }
    };

    tracing::info!(chain_length = handlers.len(), "authority chain built");
    Ok((AuthorityChain::new(handlers), staged))
}

/// Undo the connections a failed rebuild opened but never committed.
async fn retire_opened(staged: &[PooledWatcher], opened: &[usize]) {
    for &index in opened {
        staged[index].retire().await;
    }
}

/// Turn the staged watchers and the rest of the plugin list into handlers.
async fn assemble_handlers(
    config: &DoggyDnsConfig,
    targets: &[NacosTarget],
    plans: &[ReusePlan],
    staged: &[PooledWatcher],
) -> Result<Vec<(String, Arc<dyn ZoneHandler>)>> {
    let mut handlers: Vec<(String, Arc<dyn ZoneHandler>)> = Vec::new();
    let mut target_index = 0;
    for (index, plugin) in config.plugins.iter().enumerate() {
        if !plugin.enabled {
            tracing::info!(index, kind = ?plugin.kind, "skipping disabled plugin");
            continue;
        }

        match plugin.kind {
            PluginKind::Nacos => {
                let target = &targets[target_index];
                let plan = plans[target_index];
                let watcher = staged
                    .get(target_index)
                    .expect("every nacos target is staged before handlers are built");
                target_index += 1;

                let name = format!("nacos[{}]", target.zone);
                let authority = NacosAuthority::new(watcher.cache(), &target.zone, target.ttl);
                handlers.push((name.clone(), Arc::new(authority)));
                tracing::info!(
                    plugin = %name,
                    zone = %target.zone,
                    addr = %target.identity.server_addr,
                    reused = matches!(plan, ReusePlan::Existing(_)),
                    "loaded plugin"
                );
            }
            PluginKind::Native => {
                let authority = NativeAuthority::new(
                    plugin.cache_size as u64,
                    Duration::from_secs(plugin.min_ttl as u64),
                    Duration::from_secs(plugin.max_ttl as u64),
                )
                .await?;
                handlers.push(("native".to_string(), Arc::new(authority)));
                tracing::info!(plugin = "native", "loaded plugin");
            }
            PluginKind::Forward => {
                let addrs = plugin.upstream.clone().unwrap_or_else(|| {
                    DEFAULT_UPSTREAMS
                        .iter()
                        .map(|addr| addr.to_string())
                        .collect()
                });
                let name = format!("forward[{}]", addrs.join(","));
                let authority = ForwardAuthority::new(
                    addrs,
                    plugin.cache_size as u64,
                    Duration::from_secs(plugin.min_ttl as u64),
                    Duration::from_secs(plugin.max_ttl as u64),
                )
                .await?;
                handlers.push((name.clone(), Arc::new(authority)));
                tracing::info!(plugin = %name, "loaded plugin");
            }
        }
    }

    Ok(handlers)
}

async fn default_forward() -> Result<ForwardAuthority> {
    ForwardAuthority::new(
        DEFAULT_UPSTREAMS
            .iter()
            .map(|addr| addr.to_string())
            .collect(),
        1024,
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await
}

/// Feed reload requests into an orchestrator until the channel closes.
///
/// Requests are coalesced over [`RELOAD_DEBOUNCE`]: a burst of pushes (or
/// signals) arriving inside the window collapses into one reload, the newest
/// request winning because it carries the newest intent. The window exists for
/// Nacos, which delivers one callback per change of a multi-change edit, and
/// keeps a flapping operator from reconnecting plugins on every signal.
pub async fn run_reload_loop(
    orchestrator: ReloadOrchestrator,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<ReloadRequest>,
) {
    while let Some(first) = requests.recv().await {
        let mut newest = first;
        // Keep draining while requests keep arriving; a quiet window (or a
        // closed channel) ends the burst.
        while let Ok(Some(request)) = tokio::time::timeout(RELOAD_DEBOUNCE, requests.recv()).await {
            newest = request;
        }
        orchestrator.apply(newest).await;
    }
}

/// How long a burst of reload requests is folded into one.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// Subscribe to the remote config service and push changes into `requests`.
///
/// The returned watcher owns the SDK's config client, which stops delivering
/// once it is dropped — the caller must hold it for as long as remote reloads
/// should work. A failure is reported but not fatal: DNS keeps answering from
/// the config on disk, which is what the process started with.
pub async fn start_remote_config_watcher(
    config: &DoggyDnsConfig,
    requests: UnboundedSender<ReloadRequest>,
) -> Result<NacosConfigWatcher> {
    let remote = &config.remote_config;
    let watcher = NacosConfigWatcher::new(
        &remote.server_addr,
        &remote.namespace,
        &remote.group,
        &remote.data_id,
    )
    .await
    .context("failed to connect to the remote config service")?;

    watcher
        .start_watching(move |content| {
            let _ = requests.send(ReloadRequest::Nacos { content });
        })
        .await
        .context("failed to subscribe to the remote config service")?;

    tracing::info!(
        data_id = %remote.data_id,
        group = %remote.group,
        addr = %remote.server_addr,
        "remote config watcher started"
    );
    Ok(watcher)
}
