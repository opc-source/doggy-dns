use serde::Deserialize;

/// Node-side service control plane: one HTTP listener exposing the MCP
/// introspection protocol and plain health endpoints for probes.
/// The centralized admin UI lives in the separate doggy-dns-console project,
/// which consumes these endpoints.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    pub enabled: bool,
    pub listen_addr: String,
    pub port: u16,
    /// Bearer token guarding the `/mcp` endpoint only; health endpoints
    /// never require it.
    pub token: Option<String>,
    pub allowed_hosts: Vec<String>,
    /// Which endpoints the listener serves. Allowed values: `mcp`,
    /// `healthz`, `readyz`. Validated only when the plane is enabled.
    pub endpoints: Vec<String>,
}

/// The endpoint names a control plane listener may expose.
pub const CONTROL_ENDPOINTS: [&str; 3] = ["mcp", "healthz", "readyz"];

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: "127.0.0.1".to_string(),
            port: 19090,
            token: None,
            allowed_hosts: Vec::new(),
            endpoints: CONTROL_ENDPOINTS.iter().map(|s| s.to_string()).collect(),
        }
    }
}
