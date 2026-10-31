#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::{ControlConfig, MetricsTemporality, ObservabilityConfig};
use doggy_dns_observability::Observability;
use doggy_dns_plugin::RequestOutcome;
use prometheus::{Encoder, TextEncoder};

mod common;
use common::{full_config, make_dns_request};

/// With OTLP export and Prometheus pull both enabled, the two readers share
/// one instrument set. The OTLP reader is configured for delta temporality,
/// so this also proves the Prometheus reader keeps its own cumulative view: a
/// second scrape of an unchanged counter still reads the total, not a zero
/// delta.
#[tokio::test]
async fn otlp_and_prometheus_readers_share_one_cumulative_instrument() {
    let mut observability = ObservabilityConfig {
        enabled: true,
        endpoint: "http://127.0.0.1:1".to_string(), // nothing listens; export fails
        timeout_secs: 1,
        ..Default::default()
    };
    observability.metrics.temporality = MetricsTemporality::Delta;
    observability.metrics.interval_secs = 3600;
    observability.logs.enabled = false;
    observability.traces.enabled = false;

    let mut config = full_config(observability);
    config.control = ControlConfig {
        enabled: true,
        endpoints: vec!["metrics".to_string()],
        ..ControlConfig::default()
    };

    let mut telemetry = Observability::init(&config, None).expect("both readers should build");
    let registry = telemetry
        .metrics_registry()
        .expect("the control plane must get a registry");
    let metrics = telemetry
        .metrics_middleware()
        .expect("OTLP metrics must still be recorded");

    let request = make_dns_request();
    metrics.before_request(&request).await;
    metrics
        .after_request(
            &request,
            3,
            &RequestOutcome::success("native".to_string(), 1, None, None),
        )
        .await;

    let first = scrape(&registry);
    let second = scrape(&registry);
    assert_eq!(counter_value(&first), 1.0, "first scrape:\n{first}");
    assert_eq!(
        counter_value(&second),
        1.0,
        "the Prometheus reader must stay cumulative even when OTLP is delta:\n{second}"
    );

    telemetry.shutdown();
}

fn scrape(registry: &prometheus::Registry) -> String {
    let mut body = Vec::new();
    TextEncoder::new()
        .encode(&registry.gather(), &mut body)
        .expect("encode metrics");
    String::from_utf8(body).expect("text exposition is UTF-8")
}

fn counter_value(text: &str) -> f64 {
    text.lines()
        .find_map(|line| line.strip_prefix("dns_queries_total{"))
        .and_then(|rest| rest.rsplit_once(' '))
        .map(|(_, value)| value.trim().parse().expect("counter value"))
        .expect("dns_queries_total series")
}
