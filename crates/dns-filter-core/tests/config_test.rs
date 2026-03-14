#![allow(clippy::disallowed_methods)]

use dns_filter_core::config::{DnsFilterConfig, PluginKind, validate_config};

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
kind = "system_dns"
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
group = "DNS_FILTER_GROUP"
data_id = "dns-filter.toml"
"#;

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();

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

    let system_dns = &config.plugins[1];
    assert_eq!(system_dns.kind, PluginKind::SystemDns);
    assert_eq!(system_dns.cache_size, 256);
    assert_eq!(system_dns.min_ttl, 10);
    assert_eq!(system_dns.max_ttl, 300);

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

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
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

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
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

    let result: Result<DnsFilterConfig, _> = toml::from_str(toml_str);
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

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.server.worker_threads, 4);
}

#[test]
fn worker_threads_defaults_to_one() {
    let toml_str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
"#;

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
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

    let config: DnsFilterConfig = toml::from_str(toml_str).unwrap();
    let result = validate_config(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("worker_threads must be greater than 0")
    );
}
