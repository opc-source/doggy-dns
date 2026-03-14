use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct DnsFilterConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub middleware: MiddlewareConfig,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub remote_config: RemoteConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub port: u16,
    #[serde(default = "default_tcp_timeout")]
    pub tcp_timeout: u64,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout: u64,
    pub tls: Option<TlsConfig>,
    pub https: Option<HttpsConfig>,
}

fn default_tcp_timeout() -> u64 {
    10
}

fn default_shutdown_timeout() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tls_port")]
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
}

fn default_tls_port() -> u16 {
    853
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_https_port")]
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
}

fn default_https_port() -> u16 {
    443
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MiddlewareConfig {
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub metrics: bool,
    #[serde(default)]
    pub log_format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Nacos,
    SystemDns,
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
    // Resolver-specific (system_dns / forward)
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
    256
}

fn default_min_ttl() -> u32 {
    10
}

fn default_max_ttl() -> u32 {
    300
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub server_addr: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub data_id: String,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_addr: String::new(),
            namespace: String::new(),
            group: String::new(),
            data_id: String::new(),
        }
    }
}

/// Load and validate config from a TOML file path.
pub fn load_config(path: &str) -> anyhow::Result<DnsFilterConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: DnsFilterConfig = toml::from_str(&content)?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &DnsFilterConfig) -> anyhow::Result<()> {
    if let Some(tls) = &config.server.tls {
        if tls.enabled {
            anyhow::ensure!(
                std::path::Path::new(&tls.cert_path).exists(),
                "TLS cert_path does not exist: {}",
                tls.cert_path
            );
            anyhow::ensure!(
                std::path::Path::new(&tls.key_path).exists(),
                "TLS key_path does not exist: {}",
                tls.key_path
            );
        }
    }
    if let Some(https) = &config.server.https {
        if https.enabled {
            anyhow::ensure!(
                std::path::Path::new(&https.cert_path).exists(),
                "HTTPS cert_path does not exist: {}",
                https.cert_path
            );
            anyhow::ensure!(
                std::path::Path::new(&https.key_path).exists(),
                "HTTPS key_path does not exist: {}",
                https.key_path
            );
        }
    }
    if config.remote_config.enabled {
        anyhow::ensure!(
            !config.remote_config.server_addr.is_empty(),
            "remote_config.server_addr is required when remote_config is enabled"
        );
        anyhow::ensure!(
            !config.remote_config.data_id.is_empty(),
            "remote_config.data_id is required when remote_config is enabled"
        );
    }
    Ok(())
}
