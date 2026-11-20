//! Control plane wiring: the live signals it reports and the listener itself.

use doggy_dns_control::live::LiveState;
use doggy_dns_core::config::DoggyDnsConfig;
use doggy_dns_nacos::watcher::NacosServiceWatcher;
use std::sync::Arc;

/// Register the live runtime signals the control plane reports: one per
/// Nacos zone (subscription count) plus the XDP attach state.
/// Forward/native upstream health is deliberately not probed here — active
/// upstream checks are a separate concern.
pub(crate) fn build_live_state(
    watchers: &[(String, Arc<NacosServiceWatcher>)],
    xdp_attached: bool,
) -> Arc<LiveState> {
    let mut live = LiveState::new();
    for (zone, watcher) in watchers {
        let watcher = Arc::clone(watcher);
        live.register_nacos_zone(zone, move || watcher.subscribed_count() as u64);
    }
    live.register_xdp(move || xdp_attached);
    Arc::new(live)
}

pub(crate) fn start_control_plane(
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

    // A control port the previous process still holds is the expected case
    // during a restart — the DNS sockets overlap, the control port does not —
    // so the bind is retried rather than given up on. The only error left here
    // is an unusable listen address, which still must not stop DNS.
    match doggy_dns_control::start_retrying(
        state,
        live,
        control_config,
        doggy_dns_control::BIND_RETRY_INTERVAL,
        doggy_dns_core::reuseport::DNS_PORT_OVERLAP_SUPPORTED,
    ) {
        Ok(handle) => {
            tracing::info!(addr = %handle.addr, "control plane starting");
            Some(handle)
        }
        Err(error) => {
            tracing::error!(%error, "failed to start control plane; continuing with dns only");
            None
        }
    }
}
