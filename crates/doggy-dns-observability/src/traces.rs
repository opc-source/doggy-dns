use crate::auth;
use crate::endpoint::{self, Signal};
use crate::sampler::ErrorBiasedSampler;
use anyhow::{Context, Result};
use doggy_dns_basic::ObservabilityConfig;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{BatchConfigBuilder, BatchSpanProcessor, SdkTracerProvider};
use std::time::Duration;

pub(crate) fn build_tracer_provider(
    config: &ObservabilityConfig,
    resource: Resource,
) -> Result<SdkTracerProvider> {
    let endpoint = endpoint::resolve_signal_endpoint(config, Signal::Traces)?;
    let headers = auth::resolve_headers(config)?;

    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .with_timeout(Duration::from_secs(config.timeout_secs))
        .build()
        .context("failed to build OTLP span exporter")?;

    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(config.traces.max_queue_size)
        .with_max_export_batch_size(config.traces.max_export_batch_size)
        .with_scheduled_delay(Duration::from_millis(config.traces.scheduled_delay_ms))
        .build();

    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    // Normal traffic uses the configured ratio; every tracing::error! event
    // gets a forced-sampled `dns.error` child span.
    let sampler = ErrorBiasedSampler::new(config.traces.sample_ratio);

    Ok(SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(resource)
        .with_sampler(sampler)
        .build())
}
