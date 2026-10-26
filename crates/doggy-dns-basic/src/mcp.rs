use serde::Deserialize;

/// Read-only MCP server configuration for the console.
#[derive(Debug, Clone, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_mcp_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_mcp_port")]
    pub port: u16,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_mcp_listen_addr(),
            port: default_mcp_port(),
            token: None,
            allowed_hosts: Vec::new(),
        }
    }
}

fn default_mcp_listen_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_mcp_port() -> u16 {
    19090
}
