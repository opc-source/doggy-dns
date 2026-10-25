use anyhow::{Context, Result};
use doggy_dns_core::config::ObservabilityConfig;

/// One OTLP signal. All supported backends expose the same `/v1/<signal>` paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Signal {
    Logs,
    Metrics,
    Traces,
    Profiles,
}

impl Signal {
    pub(crate) fn path_segment(self) -> &'static str {
        match self {
            Self::Logs => "logs",
            Self::Metrics => "metrics",
            Self::Traces => "traces",
            Self::Profiles => "profiles",
        }
    }

    /// Signal-specific OTLP/HTTP endpoint (full URL per OTLP spec).
    fn env_endpoint(self) -> &'static str {
        match self {
            Self::Logs => "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            Self::Metrics => "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            Self::Traces => "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            Self::Profiles => "OTEL_EXPORTER_OTLP_PROFILES_ENDPOINT",
        }
    }

    fn config_endpoint(self, config: &ObservabilityConfig) -> Option<&str> {
        match self {
            Self::Logs => config.logs.endpoint.as_deref(),
            Self::Metrics => config.metrics.endpoint.as_deref(),
            Self::Traces => config.traces.endpoint.as_deref(),
            Self::Profiles => config.profiles.endpoint.as_deref(),
        }
    }
}

/// Resolve the full URL for one signal.
///
/// Precedence:
/// 1. signal-specific config endpoint (full URL)
/// 2. top-level config endpoint (base URL, `/v1/<signal>` appended)
/// 3. `OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT` (full URL)
/// 4. `OTEL_EXPORTER_OTLP_ENDPOINT` (base URL)
/// 5. backend preset default (base URL)
pub(crate) fn resolve_signal_endpoint(
    config: &ObservabilityConfig,
    signal: Signal,
) -> Result<String> {
    resolve_with_env(config, signal, |name| std::env::var(name).ok())
}

pub(crate) fn join_signal_url(base: &str, signal: Signal) -> String {
    format!(
        "{}/v1/{}",
        base.trim().trim_end_matches('/'),
        signal.path_segment()
    )
}

fn resolve_with_env(
    config: &ObservabilityConfig,
    signal: Signal,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    if let Some(endpoint) = non_empty(signal.config_endpoint(config)) {
        return validate_endpoint(endpoint.to_string());
    }

    if let Some(base) = non_empty(Some(config.endpoint.as_str())) {
        ensure_base_endpoint(base)?;
        return validate_endpoint(join_signal_url(base, signal));
    }

    if let Some(endpoint) = env(signal.env_endpoint()).filter(|value| !value.trim().is_empty()) {
        return validate_endpoint(endpoint.trim().to_string());
    }

    if let Some(base) = env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|value| !value.trim().is_empty())
    {
        ensure_base_endpoint(&base)?;
        return validate_endpoint(join_signal_url(&base, signal));
    }

    if let Some(base) = config.backend.default_endpoint() {
        return validate_endpoint(join_signal_url(base, signal));
    }

    anyhow::bail!(
        "no OTLP endpoint configured for {}: set observability.endpoint, \
         OTEL_EXPORTER_OTLP_ENDPOINT, or a signal-specific endpoint",
        signal.path_segment()
    )
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn ensure_base_endpoint(base: &str) -> Result<()> {
    let base = base.trim().trim_end_matches('/');
    anyhow::ensure!(
        !base.contains(['?', '#']),
        "OTLP base endpoint must not contain a query string or fragment"
    );
    anyhow::ensure!(
        !base.ends_with("/v1"),
        "OTLP base endpoint must not include /v1; \
         use the signal-specific endpoint for a full URL"
    );
    for signal in ["logs", "metrics", "traces", "profiles"] {
        anyhow::ensure!(
            !base.ends_with(&format!("/v1/{signal}")),
            "OTLP base endpoint must not include /v1/{signal}; \
             use the signal-specific endpoint for a full URL"
        );
    }
    Ok(())
}

fn validate_endpoint(url: String) -> Result<String> {
    let url = url.trim_end_matches('/').to_string();
    let parsed =
        reqwest::Url::parse(&url).with_context(|| format!("invalid OTLP endpoint URL: {url}"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "OTLP endpoint must use http or https: {url}"
    );
    anyhow::ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "OTLP endpoint must not contain userinfo; use basic_auth or headers: {url}"
    );
    anyhow::ensure!(
        !url.contains(char::is_whitespace),
        "OTLP endpoint must not contain whitespace: {url}"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use doggy_dns_core::config::{ObservabilityBackend, ObservabilityConfig};
    use std::collections::HashMap;

    fn config_with(
        backend: ObservabilityBackend,
        endpoint: &str,
        signal_endpoint: Option<&str>,
    ) -> ObservabilityConfig {
        let mut config = ObservabilityConfig {
            enabled: true,
            backend,
            endpoint: endpoint.to_string(),
            ..ObservabilityConfig::default()
        };
        config.logs.endpoint = signal_endpoint.map(str::to_string);
        config
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn openobserve_default_appends_signal_path() {
        let config = config_with(ObservabilityBackend::OpenObserve, "", None);
        let url = resolve_with_env(&config, Signal::Logs, no_env).unwrap();
        assert_eq!(url, "http://127.0.0.1:5080/api/default/v1/logs");
    }

    #[test]
    fn collector_preset_appends_signal_path() {
        let config = config_with(ObservabilityBackend::OtelCollector, "", None);
        let url = resolve_with_env(&config, Signal::Traces, no_env).unwrap();
        assert_eq!(url, "http://127.0.0.1:4318/v1/traces");
    }

    #[test]
    fn config_endpoint_overrides_preset() {
        let config = config_with(
            ObservabilityBackend::OpenObserve,
            "http://collector:4318/",
            None,
        );
        let url = resolve_with_env(&config, Signal::Metrics, no_env).unwrap();
        assert_eq!(url, "http://collector:4318/v1/metrics");
    }

    #[test]
    fn signal_endpoint_is_used_verbatim() {
        let config = config_with(
            ObservabilityBackend::OpenObserve,
            "http://ignored:5080/api/default",
            Some("https://collector.example.com/otlp/v1/logs"),
        );
        let url = resolve_with_env(&config, Signal::Logs, no_env).unwrap();
        assert_eq!(url, "https://collector.example.com/otlp/v1/logs");
    }

    #[test]
    fn signal_env_overrides_generic_env() {
        let config = config_with(ObservabilityBackend::Custom, "", None);
        let env: HashMap<&str, &str> = HashMap::from([
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://generic:4318"),
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://signal:4318/v1/traces",
            ),
        ]);
        let url = resolve_with_env(&config, Signal::Traces, |name| {
            env.get(name).map(|value| value.to_string())
        })
        .unwrap();
        assert_eq!(url, "http://signal:4318/v1/traces");
    }

    #[test]
    fn generic_env_endpoint_is_base_joined() {
        let config = config_with(ObservabilityBackend::Custom, "", None);
        let url = resolve_with_env(&config, Signal::Profiles, |name| {
            (name == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://generic:4318".to_string())
        })
        .unwrap();
        assert_eq!(url, "http://generic:4318/v1/profiles");
    }

    #[test]
    fn config_beats_environment() {
        let config = config_with(ObservabilityBackend::Custom, "http://config:4318", None);
        let url = resolve_with_env(&config, Signal::Logs, |name| {
            (name == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://env:4318".to_string())
        })
        .unwrap();
        assert_eq!(url, "http://config:4318/v1/logs");
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let config = config_with(ObservabilityBackend::Vector, "", None);
        let url = resolve_with_env(&config, Signal::Metrics, |_| Some("  ".to_string())).unwrap();
        assert_eq!(url, "http://127.0.0.1:4318/v1/metrics");
    }

    #[test]
    fn generic_env_endpoint_with_signal_path_is_rejected() {
        let config = config_with(ObservabilityBackend::Custom, "", None);
        let error = resolve_with_env(&config, Signal::Logs, |name| {
            (name == "OTEL_EXPORTER_OTLP_ENDPOINT")
                .then(|| "http://collector:4318/v1/logs".to_string())
        })
        .unwrap_err();
        assert!(error.to_string().contains("base endpoint"));
    }

    #[test]
    fn endpoint_userinfo_is_rejected() {
        let config = config_with(
            ObservabilityBackend::Custom,
            "http://user:pass@collector:4318",
            None,
        );
        let error = resolve_with_env(&config, Signal::Logs, no_env).unwrap_err();
        assert!(error.to_string().contains("userinfo"));
    }

    #[test]
    fn invalid_endpoint_url_is_rejected() {
        let config = config_with(ObservabilityBackend::Custom, "not-a-url", None);
        let error = resolve_with_env(&config, Signal::Logs, no_env).unwrap_err();
        assert!(error.to_string().contains("invalid OTLP endpoint"));
    }

    #[test]
    fn custom_backend_without_endpoint_fails() {
        let config = config_with(ObservabilityBackend::Custom, "", None);
        let result = resolve_with_env(&config, Signal::Logs, no_env);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no OTLP endpoint"));
    }
}
