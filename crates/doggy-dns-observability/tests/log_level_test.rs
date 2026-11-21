#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::{ObservabilityBackend, ObservabilityConfig};
use doggy_dns_observability::Observability;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use prost::Message as _;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing_subscriber::filter::EnvFilter;

mod common;
use common::{CapturedRequest, mock_otlp_server};

/// One callsite for every phase: the point of the test is that its cached
/// interest flips when the filter is replaced.
fn probe(message: &str) {
    tracing::debug!(target: "log_level_probe", "{message}");
}

/// Flush and return every log body exported since the last drain.
async fn drain_logs(
    telemetry: &Observability,
    receiver: &mut UnboundedReceiver<CapturedRequest>,
) -> Vec<String> {
    telemetry.force_flush().expect("flush should succeed");

    let mut messages = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Some(request)) = tokio::time::timeout_at(deadline, receiver.recv()).await {
        if request.path != "/v1/logs" {
            continue;
        }
        let logs = ExportLogsServiceRequest::decode(request.body.as_slice()).expect("decode logs");
        for resource_logs in &logs.resource_logs {
            for scope_logs in &resource_logs.scope_logs {
                for record in &scope_logs.log_records {
                    if let Some(
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            message,
                        ),
                    ) = record.body.as_ref().and_then(|body| body.value.as_ref())
                    {
                        messages.push(message.clone());
                    }
                }
            }
        }
    }
    messages
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_log_level_retargets_the_otlp_filter() {
    let (addr, mut receiver) = mock_otlp_server().await;

    let mut config = ObservabilityConfig {
        enabled: true,
        backend: ObservabilityBackend::Custom,
        endpoint: format!("http://{addr}"),
        service_name: Some(doggy_dns_plugin::APP_NAME.to_string()),
        ..ObservabilityConfig::default()
    };
    config.logs.stdout = false;
    config.logs.level = "info".to_string();
    config.metrics.enabled = false;
    config.traces.enabled = false;
    config.profiles.enabled = false;

    // Both outputs start from `RUST_LOG` when it parses, `logs.level` otherwise.
    let rust_log = std::env::var("RUST_LOG")
        .ok()
        .filter(|directives| EnvFilter::try_new(directives).is_ok());
    let startup = rust_log
        .clone()
        .unwrap_or_else(|| config.logs.level.clone());
    // A shell-level RUST_LOG moves the startup threshold, so the probes pinning
    // it at info only run when it is absent.
    let no_rust_log = rust_log.is_none();

    let mut telemetry = Observability::init(&common::full_config(config), None)
        .expect("observability should initialize");
    let handle = telemetry.log_level();

    let baseline = handle.current();
    assert_eq!(baseline.stdout, None, "stdout is off in this config");
    assert_eq!(baseline.otlp.as_deref(), Some(startup.as_str()));

    probe("probe before");
    tracing::info!(target: "doggy_dns_probe", "control event is exported");
    tracing::info!(target: "opentelemetry_sdk::logs", "sdk event stays out of the bridge");
    tracing::info!(target: "tracing_opentelemetry", "bridge event stays out of the bridge");
    let before = drain_logs(&telemetry, &mut receiver).await;
    if no_rust_log {
        assert!(
            !before
                .iter()
                .any(|message| message.contains("probe before")),
            "a debug event must not reach the info-level OTLP filter: {before:?}"
        );
        assert!(
            before
                .iter()
                .any(|message| message.contains("control event is exported")),
            "an ordinary info event must reach the OTLP filter: {before:?}"
        );
    }
    assert!(
        !before
            .iter()
            .any(|message| message.contains("stays out of the bridge")),
        "the OTel crates' own logs must never be re-exported: {before:?}"
    );

    let state = handle.set("debug").expect("debug should parse");
    assert_eq!(state.otlp.as_deref(), Some("debug"));
    assert_eq!(
        state.stdout, None,
        "stdout started disabled and must stay disabled"
    );
    probe("probe after");
    tracing::info!(target: "opentelemetry_sdk::logs", "sdk event stays out after set");
    let after = drain_logs(&telemetry, &mut receiver).await;
    assert!(
        after.iter().any(|message| message.contains("probe after")),
        "the retargeted filter must let the cached callsite through: {after:?}"
    );
    assert!(
        !after.iter().any(|message| message.contains("after set")),
        "a retarget must keep the OTel exclusions: {after:?}"
    );

    let state = handle.reset().expect("reset should succeed");
    assert_eq!(state.otlp.as_deref(), Some(startup.as_str()));
    assert_eq!(
        state.stdout, None,
        "stdout started disabled and must stay disabled"
    );
    probe("probe third");
    let third = drain_logs(&telemetry, &mut receiver).await;
    if no_rust_log {
        assert!(
            !third.iter().any(|message| message.contains("probe third")),
            "reset must restore the info-level filter: {third:?}"
        );
    }

    assert!(handle.set("").is_err(), "empty directives must be rejected");
    assert!(
        handle.set("nacos=notalevel").is_err(),
        "invalid directives must be rejected"
    );
    assert!(
        handle.set("opentelemetry_sdk=debug").is_err(),
        "a directive naming an OTel target must be refused: it would out-rank the bridge's exclusions"
    );
    assert_eq!(
        handle.current().otlp.as_deref(),
        Some(startup.as_str()),
        "a rejected set must leave the filter untouched"
    );

    telemetry.shutdown();
}
