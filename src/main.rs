use anyhow::{Context, Result};
use doggy_dns_control::live::LiveState;
use doggy_dns_core::config::{DoggyDnsConfig, PluginKind, load_config};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_core::middleware::logging::LoggingMiddleware;
use doggy_dns_core::server::build_server;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::NacosServiceWatcher;
use doggy_dns_observability::Observability;
use doggy_dns_plugin::Middleware;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use doggy_dns_plugin::builtin::native::NativeAuthority;
use hickory_server::zone_handler::ZoneHandler;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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

    let mut telemetry = Observability::init(
        &config.observability,
        config.middleware.log_format.as_deref(),
    )
    .context("failed to initialize observability")?;

    tracing::info!("doggy-dns starting...");

    tracing::info!(
        listen_addr = %config.server.listen_addr,
        port = config.server.port,
        worker_threads = config.server.worker_threads,
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("doggy-dns-runtime")
        .worker_threads(config.server.worker_threads)
        .build()?;

    tracing::info!(
        worker_threads = config.server.worker_threads,
        "tokio runtime built"
    );

    runtime.block_on(async_main(config, &config_content, &mut telemetry))?;

    telemetry.shutdown();
    Ok(())
}

async fn async_main(
    config: DoggyDnsConfig,
    config_content: &str,
    telemetry: &mut Observability,
) -> Result<()> {
    // Profiles need a Tokio runtime for their HTTP exporter.
    telemetry.spawn_profiles();

    // Build middleware chain
    let mut middlewares: Vec<Arc<dyn Middleware>> = Vec::new();
    if config.middleware.logging {
        middlewares.push(Arc::new(LoggingMiddleware));
    }
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

    // Build authority chain from plugins (watchers kept alive for subscriptions
    // and for the control plane's live cache signals).
    let (watchers, authority_chain) = build_authority_chain(&config).await?;

    // Create handler
    let handler = DoggyDnsHandler::new(
        middlewares,
        Arc::new(authority_chain),
        xdp.enabled(),
        AnswerRotation::new(config.server.rotate_answers),
    );

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
    let live = build_live_state(&watchers, xdp.enabled());
    let mcp_handle = start_control_plane(&config, config_content, live).await;

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

async fn build_authority_chain(
    config: &DoggyDnsConfig,
) -> Result<(Vec<(String, NacosServiceWatcher)>, AuthorityChain)> {
    let mut handlers: Vec<(String, Arc<dyn ZoneHandler>)> = Vec::new();
    let mut watchers: Vec<(String, NacosServiceWatcher)> = Vec::new();

    if config.plugins.is_empty() {
        // Default: forward to 8.8.8.8 and 1.1.1.1
        tracing::info!("no plugins configured, defaulting to forward");
        let forward = ForwardAuthority::new(
            vec!["8.8.8.8:53".to_string(), "1.1.1.1:53".to_string()],
            1024,
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await?;
        handlers.push(("forward[default]".to_string(), Arc::new(forward)));
        return Ok((watchers, AuthorityChain::new(handlers)));
    }

    for (i, plugin) in config.plugins.iter().enumerate() {
        if !plugin.enabled {
            tracing::info!(index = i, kind = ?plugin.kind, "skipping disabled plugin");
            continue;
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

                let name = format!("nacos[{}]", zone);
                let authority = NacosAuthority::new(cache, zone, plugin.ttl);
                handlers.push((name.clone(), Arc::new(authority)));
                watchers.push((zone.to_string(), watcher));
                tracing::info!(plugin = %name, zone = zone, addr = addr, "loaded plugin");
            }
            PluginKind::Native => {
                let authority = NativeAuthority::new(
                    plugin.cache_size as u64,
                    Duration::from_secs(plugin.min_ttl as u64),
                    Duration::from_secs(plugin.max_ttl as u64),
                )
                .await?;
                let name = "native".to_string();
                handlers.push((name.clone(), Arc::new(authority)));
                tracing::info!(plugin = %name, "loaded plugin");
            }
            PluginKind::Forward => {
                let addrs = plugin
                    .upstream
                    .clone()
                    .unwrap_or_else(|| vec!["8.8.8.8:53".to_string(), "1.1.1.1:53".to_string()]);
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

    tracing::info!(chain_length = handlers.len(), "authority chain built");
    Ok((watchers, AuthorityChain::new(handlers)))
}

/// Register the live runtime signals the control plane reports: one per
/// Nacos zone (cache occupancy + push freshness) plus the XDP attach state.
/// Forward/native upstream health is deliberately not probed here — active
/// upstream checks are a separate concern.
fn build_live_state(
    watchers: &[(String, NacosServiceWatcher)],
    xdp_attached: bool,
) -> Arc<LiveState> {
    let mut live = LiveState::new();
    for (zone, watcher) in watchers {
        let cache = watcher.cache();
        let last_push = watcher.last_push();
        live.register_nacos_zone(
            zone,
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
        doggy_dns_control::state::McpState::detect_hostname(),
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
