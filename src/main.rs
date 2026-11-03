use anyhow::{Context, Result};
use doggy_dns::reload::{self, Applied, ReloadCoordinator};
use doggy_dns_control::live::LiveState;
use doggy_dns_core::config::{DoggyDnsConfig, RemoteConfig, load_config};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler, SharedHandler};
use doggy_dns_core::server::build_server;
use doggy_dns_nacos::config_watcher::NacosConfigWatcher;
use doggy_dns_observability::Observability;
use doggy_dns_plugin::Middleware;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;

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
        config,
        Path::new(&config_path),
        &config_content,
        &mut telemetry,
    ))?;

    telemetry.shutdown();
    Ok(())
}

async fn async_main(
    config: DoggyDnsConfig,
    config_path: &Path,
    config_content: &str,
    telemetry: &mut Observability,
) -> Result<()> {
    // Profiles need a Tokio runtime for their HTTP exporter.
    telemetry.spawn_profiles();

    // The middlewares a reload cannot rebuild: metrics, then the XDP
    // middleware. Logging is hot, so it is composed onto these per reload.
    let mut base_middlewares: Vec<Arc<dyn Middleware>> = Vec::new();
    if let Some(metrics) = telemetry.metrics_middleware() {
        base_middlewares.push(metrics);
    }

    // Initialize the XDP fast path (Linux + `xdp` feature; a stub elsewhere).
    // The handle owns the manager, which must stay alive for the server
    // lifetime — dropping it detaches the XDP program from the interface.
    let mut xdp = xdp::init(&config, &mut base_middlewares);

    let xdp_task = xdp::start_stats(
        &mut xdp,
        telemetry.xdp_metrics(),
        Duration::from_secs(config.observability.metrics.interval_secs.max(1)),
    );

    // Build the plugin chain; startup and reload share one builder, so the
    // slots the coordinator adopts are the same objects a reload may reuse.
    let mut no_previous: Vec<reload::PluginSlot> = Vec::new();
    let slots = reload::build_chain(&config, &mut no_previous).await?;
    tracing::info!(chain_length = slots.len(), "authority chain built");

    // Create the handler behind an Arc: the server and the reload coordinator
    // share it, and the swap lives inside the handler.
    let handler = Arc::new(DoggyDnsHandler::new(
        reload::middlewares_for(config.middleware.logging, &base_middlewares),
        Arc::new(reload::authority_chain_from(&slots)),
        xdp.enabled(),
        AnswerRotation::new(config.server.rotate_answers),
    ));

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
    let mut server = build_server(&config, SharedHandler(Arc::clone(&handler))).await?;

    tracing::info!("doggy-dns is ready");

    // Start the control plane if enabled (non-fatal on failure). The XDP
    // attach flag is fixed after startup — the manager lives as long as this
    // function and dropping it detaches the program.
    let live = build_live_state(&slots, xdp.enabled());
    let mcp_handle = start_control_plane(&config, config_content, live.clone()).await;

    // The reload pipeline: both triggers hand requests to one channel, one task
    // applies them in order, and a burst inside [`reload::RELOAD_DEBOUNCE`]
    // folds into the newest request. The coordinator owns the in-force chain.
    let coordinator = Arc::new(ReloadCoordinator::new(
        config_path.to_path_buf(),
        Applied {
            raw: config_content.to_string(),
            config: config.clone(),
            slots,
            generation: 0,
        },
        Arc::clone(&handler),
        base_middlewares,
        mcp_handle.as_ref().map(|handle| handle.state.clone()),
        live,
    ));

    let (reload_tx, reload_rx) = tokio::sync::mpsc::unbounded_channel::<reload::ReloadRequest>();
    tokio::spawn(reload::run_reload_loop(Arc::clone(&coordinator), reload_rx));

    spawn_sighup_trigger(reload_tx.clone());
    spawn_remote_config_trigger(reload_tx, config.remote_config.clone());

    // Run server until shutdown signal
    let shutdown_timeout = Duration::from_secs(config.server.shutdown_timeout);
    let shutdown_token = server.shutdown_token().clone();

    // Wait for SIGINT or SIGTERM, then cancel the server. SIGHUP is handled by
    // the reload trigger above: it never shuts the resolver down.
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

    // Signal received — cancel listeners and drain with timeout
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

/// Register the live runtime signals the control plane reports: one per Nacos
/// zone the startup chain owns (cache occupancy + push freshness) plus the XDP
/// attach state. Forward/native upstream health is deliberately not probed
/// here — active upstream checks are a separate concern. After a reload the
/// coordinator replaces the zone signals from the surviving slots.
fn build_live_state(slots: &[reload::PluginSlot], xdp_attached: bool) -> Arc<LiveState> {
    let live = LiveState::new();
    live.replace_nacos_zones(reload::nacos_zone_signals(slots));
    live.register_xdp(move || xdp_attached);
    Arc::new(live)
}

/// SIGHUP: ask for a re-read of the startup config file.
///
/// The signal task only queues the request, so a reload already in progress
/// cannot make the handler miss the next signal; the pipeline applies the two
/// one after the other, and a rejected file leaves the running config untouched,
/// so an editor that writes a half-saved file cannot take the resolver down.
fn spawn_sighup_trigger(requests: tokio::sync::mpsc::UnboundedSender<reload::ReloadRequest>) {
    tokio::spawn(async move {
        let mut sighup = match signal::unix::signal(signal::unix::SignalKind::hangup()) {
            Ok(sighup) => sighup,
            Err(error) => {
                tracing::error!(%error, "cannot register SIGHUP; file-triggered reload is disabled");
                return;
            }
        };
        tracing::info!("config reload listening on SIGHUP");
        while sighup.recv().await.is_some() {
            if requests.send(reload::ReloadRequest::sighup()).is_err() {
                tracing::warn!("reload queue is closed; file-triggered reload is disabled");
                return;
            }
        }
    });
}

/// `[remote_config]`: one Nacos config watcher for the life of the process.
///
/// The SDK hands pushed content to a synchronous callback, which is exactly what
/// an unbounded channel send needs: the callback queues the request and returns,
/// and [`reload::RELOAD_DEBOUNCE`] folds a multi-line edit into one reload.
///
/// Connection failure is non-fatal and retried every 30 s, following the
/// control-plane precedent: the resolver keeps serving DNS without its remote
/// trigger.
fn spawn_remote_config_trigger(
    requests: tokio::sync::mpsc::UnboundedSender<reload::ReloadRequest>,
    remote: RemoteConfig,
) {
    if !remote.enabled {
        return;
    }
    tokio::spawn(async move {
        let _watcher = loop {
            match connect_remote_config(&remote, &requests).await {
                Ok(watcher) => break watcher,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        data_id = %remote.data_id,
                        "remote config watcher unavailable, retrying in 30s"
                    );
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };

        tracing::info!(
            data_id = %remote.data_id,
            group = %remote.group,
            "config reload listening on nacos push"
        );
        // Park holding the watcher: the SDK pushes to the listener registered on
        // this watcher's config service, so dropping it would end the trigger.
        // Shutdown cancels this task, and that is when it goes.
        std::future::pending::<()>().await;
    });
}

async fn connect_remote_config(
    remote: &RemoteConfig,
    requests: &tokio::sync::mpsc::UnboundedSender<reload::ReloadRequest>,
) -> Result<NacosConfigWatcher> {
    let watcher = NacosConfigWatcher::new(
        &remote.server_addr,
        &remote.namespace,
        &remote.group,
        &remote.data_id,
    )
    .await
    .context("failed to connect the remote config watcher")?;

    let requests = requests.clone();
    watcher
        .start_watching(move |content| {
            let _ = requests.send(reload::ReloadRequest::nacos(content));
        })
        .await
        .context("failed to subscribe to the remote config data id")?;

    Ok(watcher)
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
