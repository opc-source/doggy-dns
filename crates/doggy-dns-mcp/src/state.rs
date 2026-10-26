use chrono::{DateTime, Utc};

use doggy_dns_basic::DoggyDnsConfig;

use crate::{ConfigSnapshot, PluginSnapshot};

/// Immutable snapshot of a doggy-dns instance's state, built once at startup.
#[derive(Debug, Clone)]
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
}

impl McpState {
    /// Hostname from the environment, falling back to "unknown".
    pub fn detect_hostname() -> String {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_string())
    }

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
        }
    }
}
