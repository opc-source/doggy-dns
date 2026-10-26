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
