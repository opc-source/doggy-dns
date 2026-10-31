#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::{ControlConfig, ObservabilityConfig};
use doggy_dns_observability::Observability;
use doggy_dns_plugin::RequestOutcome;
use prometheus::{Encoder, TextEncoder};

mod common;
use common::{full_config, make_dns_request};

/// Selecting `metrics` on the control plane must collect DNS metrics even
/// with `[observability]` disabled; there is no OTLP exporter in this path.
#[tokio::test]
async fn control_metrics_endpoint_collects_without_otlp_export() {
    let mut config = full_config(ObservabilityConfig::default());
    assert!(!config.observability.enabled);
    config.control = ControlConfig {
        enabled: true,
        endpoints: vec!["metrics".to_string()],
        ..ControlConfig::default()
    };

    let mut telemetry =
        Observability::init(&config, None).expect("observability should initialize");
    let registry = telemetry
        .metrics_registry()
        .expect("the metrics endpoint must have a registry");
    let metrics = telemetry
        .metrics_middleware()
        .expect("pull-only mode must still record DNS metrics");
    assert!(
        telemetry.xdp_metrics().is_some(),
        "XDP counters must be collected"
    );

    let request = make_dns_request();
    metrics.before_request(&request).await;
    metrics
        .after_request(
            &request,
            3,
            &RequestOutcome::success("native".to_string(), 1, None, None),
        )
        .await;

    let mut body = Vec::new();
    TextEncoder::new()
        .encode(&registry.gather(), &mut body)
        .expect("encode metrics");
    let text = String::from_utf8(body).expect("text exposition is UTF-8");

    for expected in [
        "# TYPE dns_queries_total counter",
        "dns_queries_total{",
        "dns_query_duration_bucket{",
        "dns_queries_in_flight",
        "target_info{",
    ] {
        assert!(text.contains(expected), "missing {expected} in:\n{text}");
    }
    assert!(
        text.contains(r#"dns_response_code="NOERROR""#),
        "outcome attributes should be present:\n{text}"
    );
    assert!(
        !text.contains("dns_queries_ratio"),
        "unit `1` must not leak into the Prometheus name:\n{text}"
    );

    telemetry.shutdown();
}
