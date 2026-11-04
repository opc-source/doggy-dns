use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};

use doggy_dns_basic::DoggyDnsConfig;

use crate::{ConfigSnapshot, PluginSnapshot, snapshots_from};

/// The part of [`McpState`] a config reload can change: the redacted config
/// text and the plugin list shown by the MCP tools.
#[derive(Debug)]
pub struct RuntimeSnapshot {
    pub config: ConfigSnapshot,
    pub plugins: Vec<PluginSnapshot>,
}

/// Snapshot of a doggy-dns instance's state. Identity and server fields are
/// fixed at startup; the runtime part (config + plugins) is replaced whole by
/// [`McpState::update_runtime`] whenever a config reload succeeds.
#[derive(Debug)]
pub struct McpState {
    pub instance_id: String,
    pub host: String,
    pub pid: u32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub listen_addr: String,
    pub protocols: Vec<String>,
    runtime: ArcSwap<RuntimeSnapshot>,
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

    /// The runtime part as one consistent value; readers hold the `Arc` for as
    /// long as they need it and never see a torn config/plugins pair.
    pub fn runtime(&self) -> std::sync::Arc<RuntimeSnapshot> {
        self.runtime.load_full()
    }

    /// Replace the runtime snapshot after a successful config reload. Redaction
    /// happens here, exactly once per reload, on the raw content.
    pub fn update_runtime(&self, config_content: &str, plugins: &[doggy_dns_basic::PluginConfig]) {
        self.runtime.store(std::sync::Arc::new(RuntimeSnapshot {
            config: ConfigSnapshot::capture(config_content),
            plugins: snapshots_from(plugins),
        }));
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
        let runtime = ArcSwap::from_pointee(RuntimeSnapshot {
            config: ConfigSnapshot::capture(config_content),
            plugins,
        });
        Self {
            instance_id,
            host,
            pid,
            version,
            started_at,
            listen_addr,
            protocols,
            runtime,
        }
    }
}
