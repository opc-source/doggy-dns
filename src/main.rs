use anyhow::{Context, Result};
use doggy_dns_core::config::{DoggyDnsConfig, PluginKind, load_config};
use doggy_dns_core::handler::DoggyDnsHandler;
use doggy_dns_core::middleware::logging::LoggingMiddleware;
use doggy_dns_core::middleware::metrics::MetricsMiddleware;
use doggy_dns_core::server::build_server;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::NacosServiceWatcher;
use doggy_dns_plugin::Middleware;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use doggy_dns_plugin::builtin::native::NativeAuthority;
use hickory_server::zone_handler::ZoneHandler;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;

fn main() -> Result<()> {
    // Init tracing (sync, no runtime needed)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("doggy-dns starting...");

    // Load config (sync — std::fs::read_to_string)
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/doggy-dns.toml".to_string());
    let config =
        load_config(&config_path).context(format!("failed to load config from {}", config_path))?;

    tracing::info!(
        listen_addr = %config.server.listen_addr,
        port = config.server.port,
        worker_threads = config.server.worker_threads,
        tcp_timeout = config.server.tcp_timeout,
        shutdown_timeout = config.server.shutdown_timeout,
        tls_enabled = config.server.tls.as_ref().is_some_and(|t| t.enabled),
        https_enabled = config.server.https.as_ref().is_some_and(|h| h.enabled),
        logging = config.middleware.logging,
        metrics = config.middleware.metrics,
        plugin_count = config.plugins.len(),
        remote_config_enabled = config.remote_config.enabled,
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

    runtime.block_on(async_main(config))
}

async fn async_main(config: DoggyDnsConfig) -> Result<()> {
    // Build middleware chain
    let mut middlewares: Vec<Arc<dyn Middleware>> = Vec::new();
    if config.middleware.logging {
        middlewares.push(Arc::new(LoggingMiddleware));
    }
    if config.middleware.metrics {
        middlewares.push(Arc::new(MetricsMiddleware::new()));
    }

    // Build authority chain from plugins (watchers kept alive for subscriptions)
    let (_watchers, authority_chain) = build_authority_chain(&config).await?;

    // Create handler
    let handler = DoggyDnsHandler::new(middlewares, Arc::new(authority_chain));

    // Build and start server
    let mut server = build_server(&config, handler).await?;

    tracing::info!("doggy-dns is ready");

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

    Ok(())
}

async fn build_authority_chain(
    config: &DoggyDnsConfig,
) -> Result<(Vec<NacosServiceWatcher>, AuthorityChain)> {
    let mut handlers: Vec<(String, Arc<dyn ZoneHandler>)> = Vec::new();
    let mut watchers: Vec<NacosServiceWatcher> = Vec::new();

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
                watchers.push(watcher);
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
