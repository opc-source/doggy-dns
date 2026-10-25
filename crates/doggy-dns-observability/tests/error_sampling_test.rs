#![allow(clippy::disallowed_methods)]

use doggy_dns_core::config::{ObservabilityBackend, ObservabilityConfig};
use doggy_dns_observability::Observability;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::{Span, status::StatusCode};
use prost::Message as _;
use std::time::Duration;
use tracing_opentelemetry::OpenTelemetrySpanExt;

mod common;
use common::mock_otlp_server;

fn string_attribute<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(value) => {
                Some(value.as_str())
            }
            _ => None,
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bool_attribute(span: &Span, key: &str) -> Option<bool> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            opentelemetry_proto::tonic::common::v1::any_value::Value::BoolValue(value) => {
                Some(*value)
            }
            _ => None,
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracing_error_events_are_force_sampled_at_zero_ratio() {
    let (addr, mut receiver) = mock_otlp_server().await;

    let mut config = ObservabilityConfig {
        enabled: true,
        backend: ObservabilityBackend::Custom,
        endpoint: format!("http://{addr}"),
        service_name: Some("doggy-dns".to_string()),
        ..ObservabilityConfig::default()
    };
    config.logs.enabled = false;
    config.logs.stdout = false;
    config.metrics.enabled = false;
    config.traces.enabled = true;
    config.traces.sample_ratio = 0.0;
    config.traces.scheduled_delay_ms = 50;
    config.traces.max_queue_size = 128;
    config.traces.max_export_batch_size = 64;
    config.profiles.enabled = false;

    let mut telemetry =
        Observability::init(&config, None).expect("observability should initialize");

    // Ratio 0 drops the root span, but the error child must still be sampled and
    // must keep the root's trace ID.
    let root_span = tracing::info_span!("dns.request", "dns.question.name" = "example.com.");
    let root_trace_id = root_span.in_scope(|| {
        tracing::error!("forced error span: boom");
        root_span.context().span().span_context().trace_id()
    });
    drop(root_span);

    telemetry.force_flush().expect("flush should succeed");

    let mut captured = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(request)) = tokio::time::timeout_at(deadline, receiver.recv()).await {
        captured.push(request);
    }
    telemetry.shutdown();

    let traces = captured
        .iter()
        .find(|request| request.path == "/v1/traces")
        .expect("traces should be exported to /v1/traces");
    let request = ExportTraceServiceRequest::decode(traces.body.as_slice()).expect("decode traces");
    let spans = &request.resource_spans[0].scope_spans[0].spans;

    assert_eq!(
        spans.len(),
        1,
        "spans: {:?}",
        spans.iter().map(|span| &span.name).collect::<Vec<_>>()
    );
    let span = &spans[0];
    assert_eq!(span.name, "dns.error");
    assert!(
        !span.parent_span_id.is_empty(),
        "error span should be a child of the request span"
    );
    assert_eq!(span.trace_id.len(), 16);
    assert_eq!(
        hex(&span.trace_id),
        hex(&root_trace_id.to_bytes()),
        "error span must keep the original trace ID"
    );
    assert_eq!(
        span.status.as_ref().map(|status| status.code),
        Some(StatusCode::Error as i32),
        "error span must carry ERROR status"
    );
    assert_eq!(bool_attribute(span, "doggy.force_sample"), Some(true));
    assert!(
        string_attribute(span, "error.message").is_some_and(|message| message.contains("boom")),
        "error message should be kept on the forced span"
    );
    assert_eq!(
        string_attribute(span, "error.target"),
        Some("error_sampling_test")
    );
    assert!(string_attribute(span, "code.filepath").is_some());
}
