mod auth;
mod endpoint;
mod error_events;
mod logs;
mod metrics;
mod profiles;
mod resource;
mod sampler;
mod traces;

use anyhow::{Context, Result};
use doggy_dns_basic::{ObservabilityBackend, ObservabilityConfig};
use doggy_dns_plugin::Middleware;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{Level, metadata::Metadata};
use tracing_subscriber::filter::{EnvFilter, FilterFn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry, fmt};

pub use metrics::DnsMetrics;

/// Holds the OTel providers and the profile task. Drop (or call
/// [`Observability::shutdown`]) flushes pending telemetry.
pub struct Observability {
    config: ObservabilityConfig,
    logger_provider: Option<SdkLoggerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    tracer_provider: Option<SdkTracerProvider>,
    metrics: Option<Arc<DnsMetrics>>,
    profile_exporter: Option<profiles::ProfileExporter>,
    profile_task: Option<JoinHandle<()>>,
    profile_stopper: Option<Arc<profiles::ProfileStopper>>,
}

impl Observability {
    /// Build OTLP providers, install the tracing subscriber and return the
    /// telemetry guard. `log_format` comes from `middleware.log_format`
    /// (`json` or anything else for plain text).
    pub fn init(config: &ObservabilityConfig, log_format: Option<&str>) -> Result<Self> {
        let mut observability = Self {
            config: config.clone(),
            logger_provider: None,
            meter_provider: None,
            tracer_provider: None,
            metrics: None,
            profile_exporter: None,
            profile_task: None,
            profile_stopper: None,
        };

        let mut resource = None;

        if config.enabled {
            let built_resource = resource::build_resource(config);
            resource = Some(built_resource.clone());

            if config.logs.enabled {
                let provider = logs::build_logger_provider(config, built_resource.clone())?;
                observability.logger_provider = Some(provider);
            }

            if config.metrics.enabled {
                let (provider, metrics) =
                    metrics::build_meter_provider(config, built_resource.clone())?;
                observability.metrics = Some(Arc::new(metrics));
                observability.meter_provider = Some(provider);
            }

            if config.traces.enabled {
                let provider = traces::build_tracer_provider(config, built_resource.clone())?;
                observability.tracer_provider = Some(provider);
            }

            if config.profiles.enabled {
                observability.profile_exporter =
                    Some(profiles::ProfileExporter::new(config, &built_resource)?);
            }
        }

        install_subscriber(
            config,
            log_format,
            observability.logger_provider.as_ref(),
            observability.tracer_provider.as_ref(),
        )?;

        if config.enabled {
            log_startup(config, resource.as_ref());
        }

        Ok(observability)
    }

    /// Middleware that records DNS query metrics. `None` when OTLP metrics are disabled.
    pub fn metrics_middleware(&self) -> Option<Arc<dyn Middleware>> {
        self.metrics.clone().map(|metrics| metrics.middleware())
    }

    /// Start the background CPU profiler. Must be called from within a Tokio runtime.
    pub fn spawn_profiles(&mut self) {
        if self.profile_task.is_some() {
            return;
        }
        if let Some(exporter) = self.profile_exporter.take() {
            let stopper = profiles::ProfileStopper::new();
            self.profile_task = Some(exporter.spawn(Arc::clone(&stopper)));
            self.profile_stopper = Some(stopper);
        }
    }

    /// Flush all providers. Useful before shutdown and in tests.
    pub fn force_flush(&self) -> Result<()> {
        if let Some(provider) = &self.logger_provider {
            provider
                .force_flush()
                .map_err(|error| anyhow::anyhow!("failed to flush OTLP logs: {error}"))?;
        }
        if let Some(provider) = &self.meter_provider {
            provider
                .force_flush()
                .map_err(|error| anyhow::anyhow!("failed to flush OTLP metrics: {error}"))?;
        }
        if let Some(provider) = &self.tracer_provider {
            provider
                .force_flush()
                .map_err(|error| anyhow::anyhow!("failed to flush OTLP traces: {error}"))?;
        }
        Ok(())
    }

    /// Abort the profile task and shut down providers, flushing what can be flushed.
    pub fn shutdown(&mut self) {
        // Wake a blocking pprof collection thread before aborting the async task.
        if let Some(stopper) = self.profile_stopper.take() {
            stopper.stop();
        }
        if let Some(task) = self.profile_task.take() {
            task.abort();
        }

        let timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        if let Some(provider) = self.logger_provider.take() {
            let _ = provider.shutdown_with_timeout(timeout);
        }
        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown_with_timeout(timeout);
        }
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown_with_timeout(timeout);
        }
    }
}

impl Drop for Observability {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn install_subscriber(
    config: &ObservabilityConfig,
    log_format: Option<&str>,
    logger_provider: Option<&SdkLoggerProvider>,
    tracer_provider: Option<&SdkTracerProvider>,
) -> Result<()> {
    let stdout = !config.enabled || config.logs.stdout;
    let json = stdout && log_format.is_some_and(|format| format.eq_ignore_ascii_case("json"));

    if json {
        let fmt_layer = fmt::layer().json().with_filter(env_filter(stdout));
        install_layers(fmt_layer, config, logger_provider, tracer_provider)
    } else {
        let fmt_layer = fmt::layer().with_filter(env_filter(stdout));
        install_layers(fmt_layer, config, logger_provider, tracer_provider)
    }
}

fn env_filter(stdout: bool) -> EnvFilter {
    if !stdout {
        return EnvFilter::new("off");
    }
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

fn install_layers<F>(
    fmt_layer: F,
    config: &ObservabilityConfig,
    logger_provider: Option<&SdkLoggerProvider>,
    tracer_provider: Option<&SdkTracerProvider>,
) -> Result<()>
where
    F: Layer<Registry> + Send + Sync + 'static,
{
    let log_layer = match logger_provider {
        Some(provider) => {
            let level = config
                .logs
                .level
                .to_ascii_lowercase()
                .parse::<Level>()
                .with_context(|| {
                    format!("invalid observability.logs.level: {}", config.logs.level)
                })?;
            // Never forward the OTel SDK's own logs through the bridge; doing
            // so can recursively trigger another export.
            let filter = FilterFn::new(move |metadata: &Metadata<'_>| {
                *metadata.level() <= level
                    && !metadata.target().starts_with("opentelemetry")
                    && !metadata.target().starts_with("tracing_opentelemetry")
            });
            Some(OpenTelemetryTracingBridge::new(provider).with_filter(filter))
        }
        None => None,
    };

    let trace_layer = tracer_provider.map(|provider| {
        let tracer = provider.tracer(doggy_dns_plugin::APP_NAME);
        // Events are exported through the logs signal; the trace layer only
        // creates spans to avoid duplicating every log line as a span event.
        let filter = FilterFn::new(|metadata: &Metadata<'_>| {
            metadata.is_span() && *metadata.level() <= Level::INFO
        });
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(filter)
    });

    // Only meaningful when traces are enabled; the layer creates a span for
    // every tracing::error! event.
    let error_layer = tracer_provider.map(|_| error_events::ErrorEventLayer);

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(log_layer)
        .with(trace_layer)
        .with(error_layer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))
}

fn log_startup(config: &ObservabilityConfig, resource: Option<&opentelemetry_sdk::Resource>) {
    let service_name = resource
        .and_then(|resource| {
            resource
                .iter()
                .find(|(key, _)| key.as_str() == "service.name")
                .map(|(_, value)| value.to_string())
        })
        .unwrap_or_else(|| doggy_dns_plugin::APP_NAME.to_string());

    tracing::info!(
        backend = ?config.backend,
        service_name = %service_name,
        logs = config.logs.enabled,
        metrics = config.metrics.enabled,
        traces = config.traces.enabled,
        profiles = config.profiles.enabled,
        "OTLP observability enabled"
    );

    for (signal, enabled) in [
        (endpoint::Signal::Logs, config.logs.enabled),
        (endpoint::Signal::Metrics, config.metrics.enabled),
        (endpoint::Signal::Traces, config.traces.enabled),
        (endpoint::Signal::Profiles, config.profiles.enabled),
    ] {
        if enabled {
            match endpoint::resolve_signal_endpoint(config, signal) {
                Ok(url) => tracing::info!(
                    signal = signal.path_segment(),
                    endpoint = %url,
                    "OTLP signal configured"
                ),
                Err(error) => tracing::warn!(
                    signal = signal.path_segment(),
                    error = %error,
                    "OTLP signal endpoint is invalid"
                ),
            }
        }
    }

    if config.profiles.enabled && config.backend == ObservabilityBackend::Vector {
        tracing::warn!(
            "Vector's OTLP source does not support profiles; profile exports will likely be dropped"
        );
    }
    if config.profiles.enabled && config.backend == ObservabilityBackend::OtelCollector {
        tracing::warn!(
            "OTel Collector profiles require --feature-gates=+service.profilesSupport and a profiles pipeline"
        );
    }
    if config.backend == ObservabilityBackend::OpenObserve && !has_authorization(config) {
        tracing::warn!(
            "OpenObserve usually requires basic_auth or an Authorization header; none is configured"
        );
    }
    if config.profiles.enabled && config.backend == ObservabilityBackend::OpenObserve {
        tracing::warn!(
            "OpenObserve profiles support is Alpha; verify your server version supports \
             /v1/profiles and that the Profiles UI shows data"
        );
    }

    for variable in [
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_PROFILES_PROTOCOL",
    ] {
        if let Ok(value) = std::env::var(variable)
            && !value.trim().is_empty()
            && !value.trim().eq_ignore_ascii_case("http/protobuf")
        {
            tracing::warn!(
                env = variable,
                value = %value,
                "doggy-dns only supports OTLP/HTTP protobuf; ignoring this protocol env var"
            );
        }
    }
}

fn has_authorization(config: &ObservabilityConfig) -> bool {
    config.basic_auth.is_some()
        || config
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"))
        || std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
            .map(|headers| headers.to_ascii_lowercase().contains("authorization="))
            .unwrap_or(false)
}
