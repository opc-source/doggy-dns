use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use std::collections::HashMap;
use std::time::Duration;

/// Apply the OTLP/HTTP settings every signal shares: protobuf encoding, the
/// signal's own endpoint, the once-resolved headers and the export timeout.
///
/// The builder is returned unchanged, so a signal that needs more (metrics'
/// temporality) keeps configuring the concrete builder afterwards.
pub(crate) fn configured_http<B: WithExportConfig + WithHttpConfig>(
    builder: B,
    endpoint: String,
    headers: HashMap<String, String>,
    timeout_secs: u64,
) -> B {
    builder
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .with_timeout(Duration::from_secs(timeout_secs))
}
