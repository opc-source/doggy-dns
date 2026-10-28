use serde::Deserialize;

/// Node-side service control plane: one HTTP listener exposing the MCP
/// introspection protocol and plain health endpoints for probes.
/// The centralized admin UI lives in the separate doggy-dns-console project,
/// which consumes these endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_control_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_control_port")]
    pub port: u16,
    /// Bearer token guarding the `/mcp` endpoint only; health endpoints
    /// never require it.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Which endpoints the listener serves. Allowed values: `mcp`,
    /// `healthz`, `readyz`. Validated only when the plane is enabled.
    #[serde(default = "default_control_endpoints")]
    pub endpoints: Vec<String>,
}

/// The endpoint names a control plane listener may expose.
pub const CONTROL_ENDPOINTS: [&str; 3] = ["mcp", "healthz", "readyz"];

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_control_listen_addr(),
            port: default_control_port(),
            token: None,
            allowed_hosts: Vec::new(),
            endpoints: default_control_endpoints(),
        }
    }
}

fn default_control_listen_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_control_port() -> u16 {
    19090
}

fn default_control_endpoints() -> Vec<String> {
    CONTROL_ENDPOINTS.iter().map(|s| s.to_string()).collect()
}
