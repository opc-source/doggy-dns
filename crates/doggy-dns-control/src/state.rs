use chrono::{DateTime, Utc};
use std::sync::Arc;

use doggy_dns_basic::DoggyDnsConfig;

use crate::{ConfigSnapshot, LogLevelAdmin, PluginSnapshot};

/// Snapshot of a doggy-dns instance's startup state, plus the live log-filter
/// control the `set_log_level` tool operates on.
#[derive(Clone)]
pub struct McpState {
    pub instance_id: String,
    pub host: String,
    pub pid: u32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub listen_addr: String,
    pub protocols: Vec<String>,
    pub plugins: Vec<PluginSnapshot>,
    pub config: ConfigSnapshot,
    pub log_control: Arc<dyn LogLevelAdmin>,
}

impl McpState {
    /// Protocols the server exposes for this config (udp/tcp are always on).
    pub fn enabled_protocols(config: &DoggyDnsConfig) -> Vec<String> {
        let mut protos = vec!["udp".to_string(), "tcp".to_string()];
        if config.server.tls.as_ref().is_some_and(|t| t.enabled) {
            protos.push("dot".to_string());
        }
        if config.server.https.as_ref().is_some_and(|h| h.enabled) {
            protos.push("doh".to_string());
        }
        protos
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        host: String,
        pid: u32,
        version: String,
        started_at: DateTime<Utc>,
        listen_addr: String,
        protocols: Vec<String>,
        plugins: Vec<PluginSnapshot>,
        config_content: &str,
        log_control: Arc<dyn LogLevelAdmin>,
    ) -> Self {
        let instance_id = format!("{host}-{pid}");
        let config = ConfigSnapshot::capture(config_content);
        Self {
            instance_id,
            host,
            pid,
            version,
            started_at,
            listen_addr,
            protocols,
            plugins,
            config,
            log_control,
        }
    }
}
