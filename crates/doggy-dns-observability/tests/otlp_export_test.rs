#![allow(clippy::disallowed_methods)]

use doggy_dns_core::config::ObservabilityConfig;
use doggy_dns_observability::Observability;
use doggy_dns_plugin::RequestOutcome;
use hickory_proto::op::ResponseCode;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message as _;
use std::time::Duration;

mod common;
use common::{make_dns_request, mock_otlp_server, resource_attribute};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_logs_metrics_and_traces_with_openobserve_headers() {
    let (addr, mut receiver) = mock_otlp_server().await;
    let mut config = ObservabilityConfig {
        enabled: true,
        backend: doggy_dns_core::config::ObservabilityBackend::Custom,
        endpoint: format!("http://{addr}"),
        service_name: Some(doggy_dns_plugin::APP_NAME.to_string()),
        ..ObservabilityConfig::default()
    };
    config.logs.stdout = false;
    config.logs.level = "info".to_string();
    config.metrics.interval_secs = 1;
    config.traces.sample_ratio = 1.0;
    config.traces.scheduled_delay_ms = 100;
    config
        .headers
        .insert("stream-name".to_string(), "doggy-dns-test".to_string());
    config.profiles.enabled = false;

    let mut telemetry =
        Observability::init(&config, None).expect("observability should initialize");
    tracing::warn!("otlp integration log message");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let span = tracing::info_span!("dns.request", "dns.question.name" = "example.com.");
    span.in_scope(|| {});
    // The span is exported when the last handle is dropped.
    drop(span);

    let request = make_dns_request();
    let metrics = telemetry
        .metrics_middleware()
        .expect("metrics middleware should exist");
    metrics.before_request(&request).await;
    metrics
        .after_request(
            &request,
            3,
            &RequestOutcome {
                response_code: ResponseCode::NoError,
                authority: Some("native".to_string()),
                answers: 1,
                response_bytes: None,
                min_answer_ttl: None,
            },
        )
        .await;

    telemetry.force_flush().expect("flush should succeed");

    let mut captured = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(request)) = tokio::time::timeout_at(deadline, receiver.recv()).await {
        captured.push(request);
    }
    telemetry.shutdown();

    for request in &captured {
        assert_eq!(
            request.headers.get("stream-name").map(String::as_str),
            Some("doggy-dns-test"),
            "missing stream-name on {}",
            request.path
        );
        assert_eq!(
            request.headers.get("content-type").map(String::as_str),
            Some("application/x-protobuf"),
            "wrong content-type on {}",
            request.path
        );
    }

    let logs_requests = captured
        .iter()
        .filter(|request| request.path == "/v1/logs")
        .collect::<Vec<_>>();
    assert!(!logs_requests.is_empty(), "logs should be exported");

    let mut log_messages = Vec::new();
    for request in logs_requests {
        let logs = ExportLogsServiceRequest::decode(request.body.as_slice()).expect("decode logs");
        let resource_logs = &logs.resource_logs[0];
        assert_eq!(
            resource_attribute(
                &resource_logs
                    .resource
                    .as_ref()
                    .expect("resource")
                    .attributes,
                "service.name"
            )
            .as_deref(),
            Some(doggy_dns_plugin::APP_NAME)
        );
        for record in &resource_logs.scope_logs[0].log_records {
            if let Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                message,
            )) = record.body.as_ref().and_then(|body| body.value.as_ref())
            {
                log_messages.push(message.clone());
            }
        }
    }
    assert!(
        log_messages.iter().any(|message| {
            message.contains("OTLP observability enabled")
                || message.contains("otlp integration log message")
        }),
        "exported logs: {log_messages:?}"
    );

    let metrics_request = captured
        .iter()
        .find(|request| request.path == "/v1/metrics")
        .expect("metrics should be exported to /v1/metrics");
    let traces_request = captured
        .iter()
        .find(|request| request.path == "/v1/traces")
        .expect("traces should be exported to /v1/traces");

    let metrics = ExportMetricsServiceRequest::decode(metrics_request.body.as_slice())
        .expect("decode metrics");
    let metric_names = metrics.resource_metrics[0].scope_metrics[0]
        .metrics
        .iter()
        .map(|metric| metric.name.as_str())
        .collect::<Vec<_>>();
    assert!(metric_names.contains(&"dns.queries"));
    assert!(metric_names.contains(&"dns.query.duration"));
    assert!(metric_names.contains(&"dns.queries.in_flight"));

    let traces =
        ExportTraceServiceRequest::decode(traces_request.body.as_slice()).expect("decode traces");
    let span_names = traces.resource_spans[0].scope_spans[0]
        .spans
        .iter()
        .map(|span| span.name.as_str())
        .collect::<Vec<_>>();
    assert!(span_names.contains(&"dns.request"), "spans: {span_names:?}");
}
