use crate::auth;
use crate::endpoint::{self, Signal};
use anyhow::{Context, Result};
use doggy_dns_core::config::ObservabilityConfig;
use opentelemetry_otlp::{LogExporter, Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use std::time::Duration;

pub(crate) fn build_logger_provider(
    config: &ObservabilityConfig,
    resource: Resource,
) -> Result<SdkLoggerProvider> {
    let endpoint = endpoint::resolve_signal_endpoint(config, Signal::Logs)?;
    let headers = auth::resolve_headers(config)?;

    let exporter = LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .with_timeout(Duration::from_secs(config.timeout_secs))
        .build()
        .context("failed to build OTLP log exporter")?;

    Ok(SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}
