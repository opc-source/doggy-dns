use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub listen_addr: String,
    pub port: u16,
    #[serde(default = "default_tcp_timeout")]
    pub tcp_timeout: u64,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout: u64,
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
    pub tls: Option<TlsConfig>,
    pub https: Option<HttpsConfig>,
}

fn default_tcp_timeout() -> u64 {
    10
}

fn default_shutdown_timeout() -> u64 {
    5
}

fn default_worker_threads() -> usize {
    1
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
