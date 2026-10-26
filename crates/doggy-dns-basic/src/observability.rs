use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

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
