use anyhow::{Context, Result};
use doggy_dns_core::config::{DoggyDnsConfig, load_config};
use doggy_dns_core::handler::{AnswerRotation, DoggyDnsHandler};
use doggy_dns_core::middleware::logging::LoggingMiddleware;
use doggy_dns_observability::Observability;
use doggy_dns_plugin::Middleware;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;

mod control_plane;
mod dns;
mod plugins;

/// XDP fast-path wiring, chosen per platform/feature so `async_main` stays
/// cfg-free: the Linux module owns the real manager, the stub does nothing.
#[cfg(all(feature = "xdp", target_os = "linux"))]
#[path = "xdp/linux.rs"]
mod xdp;
#[cfg(not(all(feature = "xdp", target_os = "linux")))]
#[path = "xdp/stub.rs"]
mod xdp;

/// How long the background tasks and the DNS runtime's own teardown get to
/// finish before we stop waiting. `server.shutdown_timeout` bounds the DNS
/// drain; this bounds the poller, the control plane and the runtime shutdown
/// behind it.
const BACKGROUND_TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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

    // The DNS sockets are bound and served on the DNS runtime's workers; the
    // management runtime on this thread carries everything else.
    let dns_runtime = dns::build_runtime(&config)?;
    let dns_handle = dns_runtime.handle();

    let management = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build the management runtime")?;
    tracing::info!("management runtime built");

    let drained = management.block_on(async_main(
        config,
        &config_content,
        &mut telemetry,
        dns_handle,
    ))?;

    // A clean drain ends with every DNS task finished; a forced exit cancels
    // whatever is still running instead of waiting for it.
    if drained {
        dns_runtime.shutdown_timeout(BACKGROUND_TASK_SHUTDOWN_GRACE);
    } else {
        dns_runtime.shutdown_background();
    }

    telemetry.shutdown();
    Ok(())
}

/// Returns whether the DNS drain finished inside `shutdown_timeout`. `false`
/// means `main` must not wait for the DNS runtime.
async fn async_main(
    config: DoggyDnsConfig,
    config_content: &str,
    telemetry: &mut Observability,
    dns_handle: &tokio::runtime::Handle,
) -> Result<bool> {
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
    let (watchers, authority_chain) = plugins::build_authority_chain(&config).await?;

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

    // Bind and serve on the DNS runtime. This returns only once every DNS
    // socket is listening, so a bind failure is still a startup error.
    let dns::ServerHandle {
        stop: server_stop,
        mut done,
    } = dns::start_server(dns_handle, config.clone(), handler).await?;

    tracing::info!("doggy-dns is ready");

    // Start the control plane if enabled, keeping its port if the previous
    // process has not released it yet (non-fatal either way). The XDP
    // attach flag is fixed after startup — the manager lives as long as this
    // function and dropping it detaches the program.
    let live = control_plane::build_live_state(&watchers, xdp.enabled());
    let mcp_handle = control_plane::start_control_plane(&config, config_content, live);

    // Run server until shutdown signal
    let shutdown_timeout = Duration::from_secs(config.server.shutdown_timeout);

    // Wait for SIGINT or SIGTERM, then cancel the server
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = signal::ctrl_c() => tracing::info!("received SIGINT, shutting down..."),
        _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down..."),
        result = done.recv() => {
            // Server stopped on its own (socket error, etc.)
            match result {
                Some(server_result) => server_result?,
                None => anyhow::bail!("dns server task exited without reporting its result"),
            }
            tracing::info!("server stopped");
            return Ok(true);
        }
    }

    // Signal received — ask the DNS server to drain, bounded by the timeout
    let _ = server_stop.send(true);
    let drained = match tokio::time::timeout(shutdown_timeout, done.recv()).await {
        Ok(Some(server_result)) => {
            server_result?;
            tracing::info!("doggy-dns shut down gracefully");
            true
        }
        Ok(None) => anyhow::bail!("dns server task exited without reporting its result"),
        Err(_) => {
            tracing::warn!(
                "shutdown drain timed out after {}s, forcing exit",
                config.server.shutdown_timeout
            );
            false
        }
    };

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

    Ok(drained)
}

/// Await a background task's shutdown with a fixed 2-second grace, logging the
/// three outcomes (clean stop / error / timeout) under a shared label.
async fn await_shutdown<F, E>(label: &str, task: F)
where
    F: Future<Output = std::result::Result<(), E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(BACKGROUND_TASK_SHUTDOWN_GRACE, task).await {
        Ok(Ok(())) => tracing::info!("{label} shut down"),
        Ok(Err(error)) => tracing::warn!(%error, "{label} task failed during shutdown"),
        Err(_) => {
            tracing::warn!("{label} shutdown timed out after {BACKGROUND_TASK_SHUTDOWN_GRACE:?}")
        }
    }
}
