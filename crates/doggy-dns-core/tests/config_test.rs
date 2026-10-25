#![allow(clippy::disallowed_methods)]

use doggy_dns_core::config::{DoggyDnsConfig, PluginKind, validate_config};

#[test]
fn parse_full_config() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
tcp_timeout = 10

[server.tls]
enabled = true
port = 853
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[server.https]
enabled = false
port = 443
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"

[middleware]
logging = true
metrics = false

[[plugins]]
kind = "nacos"
enabled = true
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "native"
enabled = true
cache_size = 256
min_ttl = 10
max_ttl = 300

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53", "1.1.1.1:53"]
cache_size = 1024
min_ttl = 30
max_ttl = 600

[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DOGGY_DNS"
data_id = "doggy-dns.toml"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();

    assert_eq!(config.server.listen_addr, "0.0.0.0");
    assert_eq!(config.server.port, 53);
    assert_eq!(config.server.tcp_timeout, 10);

    let tls = config.server.tls.as_ref().unwrap();
    assert!(tls.enabled);
    assert_eq!(tls.port, 853);

    let https = config.server.https.as_ref().unwrap();
    assert!(!https.enabled);

    assert!(config.middleware.logging);
    assert!(!config.middleware.metrics);

    assert_eq!(config.plugins.len(), 3);

    let nacos = &config.plugins[0];
    assert_eq!(nacos.kind, PluginKind::Nacos);
    assert!(nacos.enabled);
    assert_eq!(nacos.server_addr.as_deref(), Some("127.0.0.1:8848"));
    assert_eq!(nacos.ttl, 6);

    let native = &config.plugins[1];
    assert_eq!(native.kind, PluginKind::Native);
    assert_eq!(native.cache_size, 256);
    assert_eq!(native.min_ttl, 10);
    assert_eq!(native.max_ttl, 300);

    let forward = &config.plugins[2];
    assert_eq!(forward.kind, PluginKind::Forward);
    let upstream = forward.upstream.as_ref().unwrap();
    assert_eq!(upstream.len(), 2);
    assert_eq!(upstream[0], "8.8.8.8:53");
}

#[test]
fn parse_minimal_config() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.port, 53);
    assert!(config.plugins.is_empty());
    assert!(!config.middleware.logging);
}

#[test]
fn defaults_for_optional_fields() {
    let toml_str = r#"
[server]
listen_addr = "127.0.0.1"
port = 5353
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.tcp_timeout, 10);
    assert!(config.server.tls.is_none());
    assert!(config.server.https.is_none());
    assert!(!config.remote_config.enabled);
}

#[test]
fn invalid_plugin_kind_fails() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[[plugins]]
kind = "unknown_plugin"
enabled = true
"#;

    let result: Result<DoggyDnsConfig, _> = toml::from_str(toml_str);
    assert!(result.is_err());
}

#[test]
fn worker_threads_parses_when_specified() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
worker_threads = 4
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.worker_threads, 4);
}

#[test]
fn worker_threads_defaults_to_one() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.worker_threads, 1);
}

#[test]
fn worker_threads_zero_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
worker_threads = 0
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("worker_threads must be greater than 0")
    );
}

#[test]
fn tls_enabled_nonexistent_cert_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[server.tls]
enabled = true
cert_path = "/nonexistent/cert.pem"
key_path = "/nonexistent/key.pem"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("TLS cert_path does not exist")
    );
}

#[test]
fn tls_enabled_nonexistent_key_fails_validation() {
    // Use Cargo.toml as a file that actually exists for the cert
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[server.tls]
enabled = true
cert_path = "Cargo.toml"
key_path = "/nonexistent/key.pem"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("TLS key_path does not exist")
    );
}

#[test]
fn https_enabled_nonexistent_cert_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[server.https]
enabled = true
cert_path = "/nonexistent/cert.pem"
key_path = "/nonexistent/key.pem"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("HTTPS cert_path does not exist")
    );
}

#[test]
fn remote_config_enabled_empty_server_addr_fails() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[remote_config]
enabled = true
server_addr = ""
data_id = "doggy-dns.toml"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("remote_config.server_addr is required")
    );
}

#[test]
fn remote_config_enabled_empty_data_id_fails() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[remote_config]
enabled = true
server_addr = "127.0.0.1:8848"
data_id = ""
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("remote_config.data_id is required")
    );
}

#[test]
fn tls_disabled_nonexistent_cert_passes_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[server.tls]
enabled = false
cert_path = "/nonexistent/cert.pem"
key_path = "/nonexistent/key.pem"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_ok());
}

#[test]
fn parse_observability_config() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
backend = "otel-collector"
endpoint = "http://127.0.0.1:4318"
service_name = "dns-edge"
environment = "staging"
headers = { "stream-name" = "edge" }
basic_auth = { username = "user", password = "secret" }
resource_attributes = { "service.namespace" = "dns" }

[observability.logs]
enabled = true
level = "info"
stdout = false

[observability.metrics]
enabled = true
interval_secs = 30
temporality = "delta"

[observability.traces]
enabled = true
sample_ratio = 0.25
max_queue_size = 4096
max_export_batch_size = 256
scheduled_delay_ms = 1000

[observability.profiles]
enabled = true
interval_secs = 120
duration_secs = 15
frequency_hz = 199
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let observability = &config.observability;

    assert!(observability.enabled);
    assert_eq!(
        observability.backend,
        doggy_dns_core::config::ObservabilityBackend::OtelCollector
    );
    assert_eq!(observability.endpoint, "http://127.0.0.1:4318");
    assert_eq!(observability.service_name.as_deref(), Some("dns-edge"));
    assert_eq!(observability.environment.as_deref(), Some("staging"));
    assert_eq!(observability.headers.get("stream-name").unwrap(), "edge");
    assert_eq!(observability.basic_auth.as_ref().unwrap().username, "user");
    assert_eq!(
        observability
            .resource_attributes
            .get("service.namespace")
            .unwrap(),
        "dns"
    );

    assert!(observability.logs.enabled);
    assert_eq!(observability.logs.level, "info");
    assert!(!observability.logs.stdout);

    assert_eq!(observability.metrics.interval_secs, 30);
    assert_eq!(
        observability.metrics.temporality,
        doggy_dns_core::config::MetricsTemporality::Delta
    );

    assert!((observability.traces.sample_ratio - 0.25).abs() < f64::EPSILON);
    assert_eq!(observability.traces.max_queue_size, 4096);
    assert_eq!(observability.traces.max_export_batch_size, 256);
    assert_eq!(observability.traces.scheduled_delay_ms, 1000);

    assert!(observability.profiles.enabled);
    assert_eq!(observability.profiles.interval_secs, 120);
    assert_eq!(observability.profiles.duration_secs, 15);
    assert_eq!(observability.profiles.frequency_hz, 199);

    assert!(validate_config(&config).is_ok());
}

#[test]
fn observability_defaults_are_disabled() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let observability = &config.observability;

    assert!(!observability.enabled);
    assert_eq!(
        observability.backend,
        doggy_dns_core::config::ObservabilityBackend::OpenObserve
    );
    assert_eq!(
        observability.backend.default_endpoint(),
        Some("http://127.0.0.1:5080/api/default")
    );
    assert!(observability.logs.enabled);
    assert_eq!(observability.logs.level, "warn");
    assert!(observability.logs.stdout);
    assert_eq!(observability.metrics.interval_secs, 15);
    assert!((observability.traces.sample_ratio - 0.05).abs() < f64::EPSILON);
    assert_eq!(observability.traces.max_queue_size, 8192);
    assert!(!observability.profiles.enabled);
    assert_eq!(observability.profiles.frequency_hz, 99);
}

#[test]
fn observability_without_any_signal_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true

[observability.logs]
enabled = false

[observability.metrics]
enabled = false

[observability.traces]
enabled = false
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("no signal"), "unexpected error: {error}");
}

#[test]
fn observability_invalid_sample_ratio_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default"

[observability.traces]
sample_ratio = 1.5
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("sample_ratio"), "unexpected error: {error}");
}

#[test]
fn observability_base_endpoint_with_signal_path_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default/v1/logs"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("base URL"), "unexpected error: {error}");
}

#[test]
fn observability_invalid_log_level_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default"

[observability.logs]
level = "verbose"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("logs.level"), "unexpected error: {error}");
}

#[test]
fn observability_profiles_duration_exceeding_interval_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default"

[observability.profiles]
enabled = true
interval_secs = 10
duration_secs = 20
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("duration_secs"), "unexpected error: {error}");
}

#[test]
fn observability_trace_batch_larger_than_queue_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default"

[observability.traces]
max_queue_size = 128
max_export_batch_size = 256
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(
        error.contains("max_export_batch_size"),
        "unexpected error: {error}"
    );
}

#[test]
fn observability_debug_redacts_header_values_and_password() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default"
headers = { "stream-name" = "top-secret-stream" }
basic_auth = { username = "user", password = "top-secret-password" }
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let debug = format!("{:?}", config.observability);

    assert!(!debug.contains("top-secret-stream"));
    assert!(!debug.contains("top-secret-password"));
    assert!(debug.contains("stream-name"));
}

#[test]
fn observability_base_endpoint_with_query_fails_validation() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53

[observability]
enabled = true
endpoint = "http://127.0.0.1:5080/api/default?token=secret"
"#;

    let config: DoggyDnsConfig = toml::from_str(toml_str).unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("query string"), "unexpected error: {error}");
}
