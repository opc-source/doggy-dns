//! No-op stand-in for the XDP fast path.
//!
//! Selected when the `xdp` feature is off or the target is not Linux. Mirrors
//! the surface of the Linux module so `main.rs` needs no cfg branches beyond
//! the module choice itself.

use doggy_dns_core::config::DoggyDnsConfig;
use doggy_dns_observability::XdpMetrics;
use doggy_dns_plugin::Middleware;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Nothing to keep alive: the fast path is not compiled in.
pub struct Xdp;

impl Xdp {
    pub fn enabled(&self) -> bool {
        false
    }
}

pub fn init(_config: &DoggyDnsConfig, _middlewares: &mut Vec<Arc<dyn Middleware>>) -> Xdp {
    Xdp
}

pub fn start_stats(
    _xdp: &mut Xdp,
    _metrics: Option<Arc<XdpMetrics>>,
    _period: Duration,
) -> Option<(watch::Sender<bool>, JoinHandle<()>)> {
    None
}
