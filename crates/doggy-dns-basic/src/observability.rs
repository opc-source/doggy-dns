use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;

/// The OTLP signal path segments, in the order the `/v1/<signal>` URLs spell
/// them. Both the runtime endpoint guard and config validation reject a base
/// URL that already carries one, so the list lives here once.
pub const OTLP_SIGNAL_PATHS: [&str; 4] = ["logs", "metrics", "traces", "profiles"];

/// OTLP export configuration. A single target is supported; fan-out should be
/// handled by an OTel Collector or Vector in front of the final backend.
/// Every field is optional: a missing one takes the value `Default` gives, so
/// `impl Default` below is the single source for all of them.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct ObservabilityConfig {
    pub enabled: bool,
    pub backend: ObservabilityBackend,
    /// OTLP base URL, e.g. `http://127.0.0.1:5080/api/default`. Do not append
    /// `/v1/logs` here; use the signal-specific `endpoint` for a full URL.
    pub endpoint: String,
    pub protocol: ObservabilityProtocol,
    /// Defaults to `doggy-dns`, then `OTEL_SERVICE_NAME`.
    pub service_name: Option<String>,
    /// Mapped to the `deployment.environment.name` resource attribute.
    pub environment: Option<String>,
    pub timeout_secs: u64,
    /// Extra OTLP/HTTP headers. Values support `${ENV_VAR}` interpolation.
    pub headers: BTreeMap<String, String>,
    /// Convenience for OpenObserve's `Authorization: Basic ...` header.
    pub basic_auth: Option<BasicAuthConfig>,
    /// Extra resource attributes. Config values override
    /// `OTEL_RESOURCE_ATTRIBUTES`.
    pub resource_attributes: BTreeMap<String, String>,
    pub logs: LogsConfig,
    pub metrics: MetricsConfig,
    pub traces: TracesConfig,
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
            timeout_secs: 10,
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
#[serde(default)]
pub struct LogsConfig {
    pub enabled: bool,
    /// OTLP log level. Independent of the stdout `RUST_LOG` filter.
    pub level: String,
    pub stdout: bool,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/logs`.
    pub endpoint: Option<String>,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: "warn".to_string(),
            stdout: true,
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub temporality: MetricsTemporality,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/metrics`.
    pub endpoint: Option<String>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 15,
            temporality: MetricsTemporality::default(),
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TracesConfig {
    pub enabled: bool,
    pub sample_ratio: f64,
    pub max_queue_size: usize,
    pub max_export_batch_size: usize,
    pub scheduled_delay_ms: u64,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/traces`.
    pub endpoint: Option<String>,
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_ratio: 0.05,
            max_queue_size: 8192,
            max_export_batch_size: 512,
            scheduled_delay_ms: 2000,
            endpoint: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProfilesConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub duration_secs: u64,
    pub frequency_hz: i32,
    /// Full OTLP URL override, e.g. `http://host:4318/v1/profiles`.
    pub endpoint: Option<String>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 60,
            duration_secs: 10,
            frequency_hz: 99,
            endpoint: None,
        }
    }
}
