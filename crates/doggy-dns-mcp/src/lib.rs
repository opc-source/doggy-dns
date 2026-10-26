pub mod redact;
pub mod server;
pub mod state;
pub mod transport;

pub use transport::{McpServerHandle, start};

use serde::{Deserialize, Serialize};

/// Mirror of the core MCP config, decoupled from doggy-dns-core.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            port: default_port(),
            token: None,
            allowed_hosts: Vec::new(),
        }
    }
}

fn default_listen_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    19090
}

/// Snapshot of a single plugin for the MCP `list_plugins` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSnapshot {
    pub order: usize,
    pub kind: String,
    pub enabled: bool,
    pub details: serde_json::Value,
}

/// Build an `McpState` from the individual parts.
/// Takes `pid` and `started_at` from the current process.
pub fn build_state(
    host: String,
    version: String,
    listen_addr: String,
    protocols: Vec<String>,
    plugins: Vec<PluginSnapshot>,
    config_path: std::path::PathBuf,
) -> state::McpState {
    state::McpState::from_parts(
        host,
        std::process::id(),
        version,
        chrono::Utc::now(),
        listen_addr,
        protocols,
        plugins,
        config_path,
    )
}
