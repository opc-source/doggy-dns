use anyhow::{Context, Result};
use doggy_dns::reload::{
    ReloadOrchestrator, ReloadRequest, build_chain, run_reload_loop, start_remote_config_watcher,
};
use doggy_dns_control::live::LiveState;
use doggy_dns_core::config::{DoggyDnsConfig, load_config};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_core::middleware::logging::{LoggingMiddleware, LoggingToggle};
use doggy_dns_core::server::build_server;
use doggy_dns_nacos::pool::PooledWatcher;
use doggy_dns_observability::Observability;
use doggy_dns_plugin::Middleware;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::signal;
use tokio::sync::mpsc;

/// XDP fast-path wiring, chosen per platform/feature so `async_main` stays
/// cfg-free: the Linux module owns the real manager, the stub does nothing.
#[cfg(all(feature = "xdp", target_os = "linux"))]
#[path = "xdp/linux.rs"]
mod xdp;
#[cfg(not(all(feature = "xdp", target_os = "linux")))]
#[path = "xdp/stub.rs"]
mod xdp;

fn main() -> Result<()> {
    // Load config before initializing tracing so log format and OTLP settings
    // are available to the subscriber.
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/doggy-dns.toml".to_string());
    let (config, config_content) =
        load_config(&config_path).context(format!("failed to load config from {}", config_path))?;

    let mut telemetry = Observability::init(&config, config.middleware.log_format.as_deref())
        .context("failed to initialize observability")?;

    tracing::info!("doggy-dns starting...");

    tracing::info!(
        listen_addr = %config.server.listen_addr,
        port = config.server.port,
        worker_threads = config.server.worker_threads,
        reuseport_listen_num = config.server.reuseport_listen_num,
        cpu_affinity = ?config.server.cpu_affinity,
        tcp_timeout = config.server.tcp_timeout,
        shutdown_timeout = config.server.shutdown_timeout,
        tls_enabled = config.server.tls.as_ref().is_some_and(|t| t.enabled),
        https_enabled = config.server.https.as_ref().is_some_and(|h| h.enabled),
        logging = config.middleware.logging,
        plugin_count = config.plugins.len(),
        remote_config_enabled = config.remote_config.enabled,
        observability_enabled = config.observability.enabled,
        "loaded configuration"
    );

    for (i, plugin) in config.plugins.iter().enumerate() {
        tracing::info!(
            index = i,
            kind = ?plugin.kind,
            enabled = plugin.enabled,
            "plugin configured"
        );
    }

    // Build runtime with configured worker threads
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .thread_name("doggy-dns-runtime")
        .worker_threads(config.server.worker_threads);

    if let Some(affinity) = &config.server.cpu_affinity {
        configure_cpu_affinity(&mut builder, affinity)?;
    }

    let runtime = builder.build()?;

    tracing::info!(
        worker_threads = config.server.worker_threads,
        "tokio runtime built"
    );

    runtime.block_on(async_main(
        config_path,
        config,
        &config_content,
        &mut telemetry,
    ))?;

    telemetry.shutdown();
    Ok(())
}

async fn async_main(
    config_path: String,
    config: DoggyDnsConfig,
    config_content: &str,
    telemetry: &mut Observability,
) -> Result<()> {
    // Profiles need a Tokio runtime for their HTTP exporter.
    telemetry.spawn_profiles();

    // Build middleware chain. Query logging is installed unconditionally and
    // switched by a toggle rather than pushed conditionally:
    // `middleware.logging` is reloadable, while the chain itself cannot be
    // rebuilt in place because it owns the live XDP cache map and the metrics
    // instruments.
    let logging = LoggingToggle::new(config.middleware.logging);
    let mut middlewares: Vec<Arc<dyn Middleware>> =
        vec![Arc::new(LoggingMiddleware::with_toggle(logging.clone()))];
    if let Some(metrics) = telemetry.metrics_middleware() {
        middlewares.push(metrics);
    }

    // Initialize the XDP fast path (Linux + `xdp` feature; a stub elsewhere).
    // The handle owns the manager, which must stay alive for the server
    // lifetime — dropping it detaches the XDP program from the interface.
    let mut xdp = xdp::init(&config, &mut middlewares);

    let xdp_task = xdp::start_stats(
        &mut xdp,
        telemetry.xdp_metrics(),
        Duration::from_secs(config.observability.metrics.interval_secs.max(1)),
    );

    // Build authority chain from plugins. Nacos watchers are pooled by the
    // connection they opened so a reload can reuse them, and they feed the
    // control plane's live cache signals.
    let (authority_chain, pool) = build_chain(&config, &[])
        .await
        .context("failed to build authority chain")?;

    // Create handler
    let handler = DoggyDnsHandler::new(
        middlewares,
        Arc::new(authority_chain),
        xdp.enabled(),
        AnswerRotation::new(config.server.rotate_answers),
    );

    // The handler moves into the server below and hickory owns it for the rest
    // of the process, so take the handle to its reloadable halves now.
    let reload = handler.reload_handle();

    // The live signals capture the pooled caches at this moment; a reload that
    // opens a new Nacos connection is only reported after a restart.
    let live = build_live_state(&pool, xdp.enabled());

    // Every reload trigger (SIGHUP, remote pushes) funnels through one channel
    // into one task, so two sources cannot rebuild the chain concurrently.
    let (reload_tx, reload_rx) = mpsc::unbounded_channel::<ReloadRequest>();
    let orchestrator = ReloadOrchestrator::new(config_path, config.clone(), pool, reload, logging);
    tokio::spawn(run_reload_loop(orchestrator, reload_rx));

    // The kernel answers a cache hit without asking userspace again, so the
    // record order frozen into the cached reply is the one every hit replays
    // until the entry expires. Say it once, at startup, rather than letting it
    // look like rotation is broken.
    if xdp.enabled() && config.server.rotate_answers {
        tracing::warn!(
            "answer rotation applies to userspace answers only: an XDP cache hit replays the \
             order captured when the entry was written until its TTL expires"
        );
    }

    // Build and start server
    let mut server = build_server(&config, handler).await?;

    tracing::info!("doggy-dns is ready");

    // Start the control plane if enabled (non-fatal on failure). The XDP
    // attach flag is fixed after startup — the manager lives as long as this
    // function and dropping it detaches the program.
    let mcp_handle = start_control_plane(&config, config_content, live).await;

    // SIGHUP asks for a reload and never shuts anything down, so it is handled
    // in its own task instead of the shutdown select below. The one-shot cue
    // ends that task at shutdown, releasing its copy of the reload sender.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let sighup_tx = reload_tx.clone();
    tokio::spawn(async move {
        watch_sighup(sighup_tx, stop_rx).await;
    });

    // A pushed remote config is a reload request as well. The watcher owns the
    // SDK config client, so it is held until shutdown: dropping it stops
    // delivery. Failing to reach the config service is non-fatal — DNS keeps
    // serving the config it loaded from disk.
    let mut remote_watcher = None;
    if config.remote_config.enabled {
        match start_remote_config_watcher(&config, reload_tx.clone()).await {
            Ok(watcher) => remote_watcher = Some(watcher),
            Err(error) => {
                tracing::error!(%error, "remote config unavailable; reloads will be local only");
            }
        }
    }

    // Run server until shutdown signal
    let shutdown_timeout = Duration::from_secs(config.server.shutdown_timeout);
    let shutdown_token = server.shutdown_token().clone();

    // Wait for SIGINT or SIGTERM, then cancel the server
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = signal::ctrl_c() => tracing::info!("received SIGINT, shutting down..."),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down..."),
        result = server.block_until_done() => {
            // Server stopped on its own (socket error, etc.)
            result?;
            tracing::info!("server stopped");
            return Ok(());
        }
    }

    // Signal received — stop the background reload plumbing first: the cue
    // ends the SIGHUP task, dropping the watcher stops remote pushes, and the
    // last sender copy going lets the reload loop finish.
    let _ = stop_tx.send(());
    drop(remote_watcher);
    drop(reload_tx);

    // cancel listeners and drain with timeout
    shutdown_token.cancel();
    match tokio::time::timeout(shutdown_timeout, server.block_until_done()).await {
        Ok(result) => result?,
        Err(_) => tracing::warn!(
            "shutdown drain timed out after {}s, forcing exit",
            config.server.shutdown_timeout
        ),
    }
    tracing::info!("doggy-dns shut down gracefully");

    // Stop the stats poller and let it perform its final map read before the
    // telemetry providers are flushed in main().
    if let Some((stop, handle)) = xdp_task {
        let _ = stop.send(true);
        await_shutdown("xdp stats poller", handle).await;
    }

    // Shut down the control plane listener
    if let Some(handle) = mcp_handle {
        handle.cancel.cancel();
        await_shutdown("mcp server", handle.wait()).await;
    }

    Ok(())
}

/// Configure CPU affinity for tokio worker threads.
///
/// Each worker thread is pinned to a specific CPU core in round-robin order.
/// The vector length should match `worker_threads`.
/// Panics if any core index exceeds the available CPU count.
fn configure_cpu_affinity(
    builder: &mut tokio::runtime::Builder,
    affinity: &Vec<usize>,
) -> Result<()> {
    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    for &core_idx in affinity {
        anyhow::ensure!(
            core_idx < core_ids.len(),
            "cpu_affinity contains core index {core_idx}, but only {} cores available",
            core_ids.len()
        );
    }

    tracing::info!(cpu_affinity = ?affinity, "CPU affinity configured");
    let affinity = affinity.clone();
    builder.on_thread_start(move || {
        static INDEX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let idx = INDEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % affinity.len();
        if let Some(core_id) = affinity.get(idx).and_then(|&id| core_ids.get(id)).copied()
            && core_affinity::set_for_current(core_id)
        {
            tracing::info!(
                worker_index = idx,
                core_id = core_id.id,
                "pinned worker thread to CPU core"
            );
        }
    });

    Ok(())
}

/// Await a background task's shutdown with a fixed 2-second grace, logging the
/// three outcomes (clean stop / error / timeout) under a shared label.
async fn await_shutdown<F, E>(label: &str, task: F)
where
    F: Future<Output = std::result::Result<(), E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(Duration::from_secs(2), task).await {
        Ok(Ok(())) => tracing::info!("{label} shut down"),
        Ok(Err(error)) => tracing::warn!(%error, "{label} task failed during shutdown"),
        Err(_) => tracing::warn!("{label} shutdown timed out after 2s"),
    }
}

/// Deliver every SIGHUP as a reload request, until the shutdown cue arrives
/// or the receiver is gone.
async fn watch_sighup(
    sender: mpsc::UnboundedSender<ReloadRequest>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut sighup = signal::unix::signal(signal::unix::SignalKind::hangup())
        .expect("failed to register SIGHUP handler");
    loop {
        tokio::select! {
            _ = &mut stop => break,
            signal = sighup.recv() => {
                if signal.is_none() {
                    break;
                }
                tracing::info!("received SIGHUP, reloading configuration");
                if sender.send(ReloadRequest::Sighup).is_err() {
                    break;
                }
            }
        }
    }
}

/// Register the live runtime signals the control plane reports: one per
/// Nacos zone (cache occupancy + push freshness) plus the XDP attach state.
/// Forward/native upstream health is deliberately not probed here — active
/// upstream checks are a separate concern.
fn build_live_state(pool: &[PooledWatcher], xdp_attached: bool) -> Arc<LiveState> {
    let mut live = LiveState::new();
    for watcher in pool {
        let cache = watcher.cache();
        let last_push = watcher.last_push();
        live.register_nacos_zone(
            &watcher.zone,
            move || cache.len() as u64,
            move || last_push.load(Ordering::Relaxed),
        );
    }
    live.register_xdp(move || xdp_attached);
    Arc::new(live)
}

async fn start_control_plane(
    config: &DoggyDnsConfig,
    config_content: &str,
    live: Arc<LiveState>,
) -> Option<doggy_dns_control::ControlPlaneHandle> {
    if !config.control.enabled {
        return None;
    }

    let state = Arc::new(doggy_dns_control::build_state(
        // The control plane reports this hostname as its identity; the OTel
        // resource derives the same value for `host.name`.
        doggy_dns_observability::detect_hostname(),
        env!("CARGO_PKG_VERSION").to_string(),
        doggy_dns_core::config::net::socket_addr(&config.server.listen_addr, config.server.port)
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| format!("{}:{}", config.server.listen_addr, config.server.port)),
        doggy_dns_control::state::McpState::enabled_protocols(config),
        doggy_dns_control::snapshots_from(&config.plugins),
        config_content,
    ));

    let control_config = config.control.clone();

    // start() only binds the listener and spawns the serve task; a failure
    // here must stay non-fatal so DNS service continues alone.
    match doggy_dns_control::start(state, live, control_config).await {
        Ok(handle) => {
            tracing::info!(addr = %handle.addr, "control plane listening");
            Some(handle)
        }
        Err(error) => {
            tracing::error!(%error, "failed to start control plane; continuing with dns only");
            None
        }
    }
}
