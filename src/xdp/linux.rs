//! XDP fast-path wiring for Linux builds with the `xdp` feature.
//!
//! Selected by cfg in `main.rs`; the stub counterpart keeps the same surface
//! so `async_main` itself stays free of cfg branches.

use doggy_dns_core::config::DoggyDnsConfig;
use doggy_dns_observability::XdpMetrics;
use doggy_dns_plugin::Middleware;
use doggy_dns_xdp::{XdpCacheMiddleware, XdpManager};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Owns the [`XdpManager`].
///
/// The manager must stay alive for the server lifetime — dropping it detaches
/// the XDP program from the interface.
pub struct Xdp(Option<XdpManager>);

impl Xdp {
    /// Whether the kernel fast path is active.
    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }
}

/// Initialize the XDP fast-path middleware and manager.
///
/// On success the cache middleware is appended to `middlewares`. A failed
/// initialization is logged and degrades to running without the fast path.
pub fn init(config: &DoggyDnsConfig, middlewares: &mut Vec<Arc<dyn Middleware>>) -> Xdp {
    if !config.xdp.enabled {
        return Xdp(None);
    }
    match XdpManager::new(&config.xdp, config.server.port) {
        Ok(mut mgr) => {
            let cache = mgr.take_cache();
            let middleware = XdpCacheMiddleware::new(cache, &config.xdp);
            middlewares.push(Arc::new(middleware));
            // Drain kernel-side failure records into tracing. The task is
            // cancelled with the runtime, so it needs no shutdown handle.
            if let Some(logger) = mgr.take_log() {
                doggy_dns_xdp::kernel_log::spawn_log_flush(logger);
            }
            tracing::info!(
                interface = %mgr.interface,
                attach_mode = %config.xdp.attach_mode,
                "XDP fast path enabled"
            );
            Xdp(Some(mgr))
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "failed to initialize XDP fast path, continuing without it"
            );
            Xdp(None)
        }
    }
}

/// Start the XDP counter poller when the fast path and OTLP metrics are both on.
///
/// Returns the stop sender and the task handle. The closure keeps the
/// observability crate independent of the XDP crate: only this module knows
/// both.
pub fn start_stats(
    xdp: &mut Xdp,
    metrics: Option<Arc<XdpMetrics>>,
    period: Duration,
) -> Option<(watch::Sender<bool>, JoinHandle<()>)> {
    let manager = xdp.0.as_mut()?;
    // Take the reader even when metrics are off so the state machine matches
    // the original single-shot take.
    let reader = manager.take_stats();
    let Some(metrics) = metrics else {
        tracing::warn!(
            "XDP fast path is enabled but OTLP metrics are disabled; \
             kernel-side counters will not be exported"
        );
        return None;
    };

    let (stop, stop_rx) = watch::channel(false);
    let handle =
        doggy_dns_xdp::stats::spawn_stats_poller(reader, period, stop_rx, move |label, count| {
            metrics.record(label, count)
        });
    Some((stop, handle))
}
