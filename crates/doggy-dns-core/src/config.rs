use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Deserialize)]
pub struct DoggyDnsConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub middleware: MiddlewareConfig,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub remote_config: RemoteConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
}

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

#[derive(Debug, Clone, Default, Deserialize)]
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

/// OTLP export configuration. A single target is supported; fan-out should be
/// handled by an OTel Collector or Vector in front of the final backend.
#[derive(Clone, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub backend: ObservabilityBackend,
    /// OTLP base URL, e.g. `http://127.0.0.1:5080/api/default`. Do not append
    /// `/v1/logs` here; use the signal-specific `endpoint` for a full URL.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub protocol: ObservabilityProtocol,
    /// Defaults to `doggy-dns`, then `OTEL_SERVICE_NAME`.
    #[serde(default)]
    pub service_name: Option<String>,
    /// Mapped to the `deployment.environment.name` resource attribute.
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default = "default_observability_timeout")]
    pub timeout_secs: u64,
    /// Extra OTLP/HTTP headers. Values support `${ENV_VAR}` interpolation.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Convenience for OpenObserve's `Authorization: Basic ...` header.
    #[serde(default)]
    pub basic_auth: Option<BasicAuthConfig>,
    /// Extra resource attributes. Config values override
    /// `OTEL_RESOURCE_ATTRIBUTES`.
    #[serde(default)]
    pub resource_attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub logs: LogsConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub traces: TracesConfig,
    #[serde(default)]
    pub profiles: ProfilesConfig,
}

impl fmt::Debug for ObservabilityConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservabilityConfig")
            .field("enabled", &self.enabled)
            .field("backend", &self.backend)
            .field("endpoint", &self.endpoint)
            .field("protocol", &self.protocol)
            .field("service_name", &self.service_name)
            .field("environment", &self.environment)
            .field("timeout_secs", &self.timeout_secs)
            .field(
                "header_names",
                &self.headers.keys().collect::<Vec<&String>>(),
            )
            .field("basic_auth", &self.basic_auth)
            .field("resource_attributes", &self.resource_attributes)
            .field("logs", &self.logs)
            .field("metrics", &self.metrics)
            .field("traces", &self.traces)
            .field("profiles", &self.profiles)
            .finish()
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: ObservabilityBackend::default(),
            endpoint: String::new(),
            protocol: ObservabilityProtocol::default(),
            service_name: None,
            environment: None,
            timeout_secs: default_observability_timeout(),
            headers: BTreeMap::new(),
            basic_auth: None,
            resource_attributes: BTreeMap::new(),
            logs: LogsConfig::default(),
            metrics: MetricsConfig::default(),
            traces: TracesConfig::default(),
            profiles: ProfilesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ObservabilityBackend {
    #[default]
    #[serde(rename = "openobserve")]
    OpenObserve,
    #[serde(rename = "otel-collector")]
    OtelCollector,
    #[serde(rename = "vector")]
    Vector,
    #[serde(rename = "custom")]
    Custom,
}

impl ObservabilityBackend {
    /// Default OTLP/HTTP base URL for this backend.
    pub fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::OpenObserve => Some("http://127.0.0.1:5080/api/default"),
            Self::OtelCollector | Self::Vector => Some("http://127.0.0.1:4318"),
            Self::Custom => None,
        }
    }

    /// Vector's OTLP source does not support profiles.
    pub fn supports_profiles(self) -> bool {
        !matches!(self, Self::Vector)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ObservabilityProtocol {
    #[default]
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricsTemporality {
    #[default]
    Cumulative,
    Delta,
}

#[derive(Clone, Deserialize)]
pub struct BasicAuthConfig {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for BasicAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasicAuthConfig")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// OTLP log level. Independent of the stdout `RUST_LOG` filter.
    #[serde(default = "default_logs_level")]
    pub level: String,
    #[serde(default = "default_true")]
    pub stdout: bool,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/logs`.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: default_logs_level(),
            stdout: true,
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_metrics_interval")]
    pub interval_secs: u64,
    #[serde(default)]
    pub temporality: MetricsTemporality,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/metrics`.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_metrics_interval(),
            temporality: MetricsTemporality::default(),
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TracesConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_sample_ratio")]
    pub sample_ratio: f64,
    #[serde(default = "default_trace_queue_size")]
    pub max_queue_size: usize,
    #[serde(default = "default_trace_batch_size")]
    pub max_export_batch_size: usize,
    #[serde(default = "default_trace_scheduled_delay")]
    pub scheduled_delay_ms: u64,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/traces`.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_ratio: default_sample_ratio(),
            max_queue_size: default_trace_queue_size(),
            max_export_batch_size: default_trace_batch_size(),
            scheduled_delay_ms: default_trace_scheduled_delay(),
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfilesConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_profiles_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_profiles_duration")]
    pub duration_secs: u64,
    #[serde(default = "default_profiles_frequency")]
    pub frequency_hz: i32,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/profiles`.
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_profiles_interval(),
            duration_secs: default_profiles_duration(),
            frequency_hz: default_profiles_frequency(),
            endpoint: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_observability_timeout() -> u64 {
    10
}

fn default_logs_level() -> String {
    "warn".to_string()
}

fn default_metrics_interval() -> u64 {
    15
}

fn default_sample_ratio() -> f64 {
    0.05
}

fn default_trace_queue_size() -> usize {
    8192
}

fn default_trace_batch_size() -> usize {
    512
}

fn default_trace_scheduled_delay() -> u64 {
    2000
}

fn default_profiles_interval() -> u64 {
    60
}

fn default_profiles_duration() -> u64 {
    10
}

fn default_profiles_frequency() -> i32 {
    99
}

/// Load and validate config from a TOML file path.
pub fn load_config(path: &str) -> anyhow::Result<DoggyDnsConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: DoggyDnsConfig = toml::from_str(&content)?;
    validate_config(&config)?;
    Ok(config)
}

pub fn validate_config(config: &DoggyDnsConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.server.worker_threads > 0,
        "server.worker_threads must be greater than 0"
    );
    if let Some(tls) = &config.server.tls
        && tls.enabled
    {
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
    if let Some(https) = &config.server.https
        && https.enabled
    {
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
    validate_observability(&config.observability)?;
    Ok(())
}

fn validate_observability(config: &ObservabilityConfig) -> anyhow::Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if let Some(name) = &config.service_name {
        anyhow::ensure!(
            !name.trim().is_empty(),
            "observability.service_name must not be empty"
        );
    }

    let any_signal = config.logs.enabled
        || config.metrics.enabled
        || config.traces.enabled
        || config.profiles.enabled;
    anyhow::ensure!(
        any_signal,
        "observability.enabled is true but no signal (logs/metrics/traces/profiles) is enabled"
    );

    if !config.endpoint.trim().is_empty() {
        validate_observability_url("observability.endpoint", &config.endpoint, false)?;
    }
    for (name, endpoint) in [
        (
            "observability.logs.endpoint",
            config.logs.endpoint.as_deref(),
        ),
        (
            "observability.metrics.endpoint",
            config.metrics.endpoint.as_deref(),
        ),
        (
            "observability.traces.endpoint",
            config.traces.endpoint.as_deref(),
        ),
        (
            "observability.profiles.endpoint",
            config.profiles.endpoint.as_deref(),
        ),
    ] {
        if let Some(endpoint) = endpoint
            && !endpoint.trim().is_empty()
        {
            validate_observability_url(name, endpoint, true)?;
        }
    }

    anyhow::ensure!(
        config.timeout_secs > 0,
        "observability.timeout_secs must be greater than 0"
    );

    let level = config.logs.level.to_ascii_lowercase();
    anyhow::ensure!(
        matches!(
            level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ),
        "observability.logs.level must be one of trace, debug, info, warn, error"
    );

    anyhow::ensure!(
        config.metrics.interval_secs > 0,
        "observability.metrics.interval_secs must be greater than 0"
    );

    anyhow::ensure!(
        config.traces.sample_ratio.is_finite() && (0.0..=1.0).contains(&config.traces.sample_ratio),
        "observability.traces.sample_ratio must be between 0.0 and 1.0"
    );
    anyhow::ensure!(
        config.traces.max_queue_size > 0,
        "observability.traces.max_queue_size must be greater than 0"
    );
    anyhow::ensure!(
        config.traces.max_export_batch_size > 0
            && config.traces.max_export_batch_size <= config.traces.max_queue_size,
        "observability.traces.max_export_batch_size must be in 1..=max_queue_size"
    );
    anyhow::ensure!(
        config.traces.scheduled_delay_ms > 0,
        "observability.traces.scheduled_delay_ms must be greater than 0"
    );

    anyhow::ensure!(
        config.profiles.interval_secs > 0,
        "observability.profiles.interval_secs must be greater than 0"
    );
    anyhow::ensure!(
        config.profiles.duration_secs > 0
            && config.profiles.duration_secs <= config.profiles.interval_secs,
        "observability.profiles.duration_secs must be in 1..=interval_secs"
    );
    anyhow::ensure!(
        (1..=1000).contains(&config.profiles.frequency_hz),
        "observability.profiles.frequency_hz must be between 1 and 1000"
    );
    if config.profiles.enabled {
        anyhow::ensure!(
            cfg!(any(target_os = "linux", target_os = "macos")),
            "observability.profiles is only supported on Linux and macOS"
        );
    }

    if let Some(auth) = &config.basic_auth {
        anyhow::ensure!(
            !auth.username.trim().is_empty() && !auth.password.is_empty(),
            "observability.basic_auth username and password must not be empty"
        );
    }

    Ok(())
}

fn validate_observability_url(
    name: &str,
    value: &str,
    allow_signal_path: bool,
) -> anyhow::Result<()> {
    let value = value.trim();
    anyhow::ensure!(
        value.starts_with("http://") || value.starts_with("https://"),
        "{name} must start with http:// or https://"
    );
    anyhow::ensure!(
        !value.contains(char::is_whitespace),
        "{name} must not contain whitespace"
    );
    if !allow_signal_path {
        anyhow::ensure!(
            !value.contains(['?', '#']),
            "{name} must not contain a query string or fragment"
        );
        anyhow::ensure!(
            !value.trim_end_matches('/').ends_with("/v1"),
            "{name} must be the OTLP base URL without /v1"
        );
        for signal in ["logs", "metrics", "traces", "profiles"] {
            anyhow::ensure!(
                !value.ends_with(&format!("/v1/{signal}")),
                "{name} must be the OTLP base URL without /v1/{signal}; use the signal-specific endpoint for a full URL"
            );
        }
    }
    Ok(())
}
