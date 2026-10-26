use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Nacos,
    Native,
    Forward,
}

/// Flat plugin config. Each field is optional; only the fields relevant
/// to the plugin's `kind` are used. Unknown fields for a given kind
/// are simply ignored at runtime.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginConfig {
    pub kind: PluginKind,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    // Nacos-specific
    pub server_addr: Option<String>,
    pub namespace: Option<String>,
    pub group: Option<String>,
    pub dns_zone: Option<String>,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    // Resolver-specific (native / forward)
    #[serde(default = "default_cache_size")]
    pub cache_size: u32,
    #[serde(default = "default_min_ttl")]
    pub min_ttl: u32,
    #[serde(default = "default_max_ttl")]
    pub max_ttl: u32,
    // Forward-specific
    pub upstream: Option<Vec<String>>,
}

fn default_enabled() -> bool {
    true
}

fn default_ttl() -> u32 {
    6
}

fn default_cache_size() -> u32 {
    4096
}

fn default_min_ttl() -> u32 {
    10
}

fn default_max_ttl() -> u32 {
    300
}
