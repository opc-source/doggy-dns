#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::ObservabilityConfig;
use doggy_dns_observability::Observability;

mod common;

#[test]
fn stdout_stays_disabled_when_export_is_disabled() {
    // `enabled = false` used to force stdout on, overriding an explicit
    // `logs.stdout = false`. The config is the only thing that decides now.
    let mut config = ObservabilityConfig {
        enabled: false,
        ..ObservabilityConfig::default()
    };
    config.logs.stdout = false;

    let telemetry = Observability::init(&common::full_config(config), None)
        .expect("observability should initialize");

    assert_eq!(
        telemetry.log_level().current().stdout,
        None,
        "logs.stdout = false must stay off even when OTLP export is disabled"
    );
}
