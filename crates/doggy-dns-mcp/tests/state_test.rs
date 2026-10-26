#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_basic::PluginConfig;
use doggy_dns_mcp::state::McpState;

#[test]
fn instance_id_contains_host_and_pid() {
    let state = McpState::from_parts(
        "dns-node-1".into(),
        4242,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        Vec::new(),
        "",
    );
    assert_eq!(state.instance_id, "dns-node-1-4242");
    assert_eq!(state.host, "dns-node-1");
    assert_eq!(state.pid, 4242);
}

#[test]
fn state_preserves_protocols_and_listen_addr() {
    let state = McpState::from_parts(
        "test-host".into(),
        1234,
        "0.2.0".into(),
        Utc::now(),
        "127.0.0.1:5353".into(),
        vec!["udp".into(), "tcp".into(), "dot".into(), "doh".into()],
        Vec::new(),
        "",
    );
    assert_eq!(state.version, "0.2.0");
    assert_eq!(state.listen_addr, "127.0.0.1:5353");
    assert_eq!(state.protocols.len(), 4);
    assert_eq!(state.protocols[2], "dot");
}

#[test]
fn snapshots_from_applies_kind_defaults_and_order() {
    let toml_str = r#"
[[plugins]]
kind = "nacos"
enabled = false

[[plugins]]
kind = "forward"
upstream = ["8.8.8.8:53"]
max_ttl = 600

[[plugins]]
kind = "native"
"#;
    #[derive(serde::Deserialize)]
    struct Wrapper {
        plugins: Vec<PluginConfig>,
    }
    let parsed: Wrapper = toml::from_str(toml_str).unwrap();
    let snapshots = doggy_dns_mcp::snapshots_from(&parsed.plugins);

    assert_eq!(snapshots.len(), 3);
    assert_eq!(snapshots[0].order, 0);
    assert_eq!(snapshots[0].kind, "nacos");
    assert!(!snapshots[0].enabled);
    assert_eq!(snapshots[0].details["serverAddr"], "127.0.0.1:8848");
    assert_eq!(snapshots[0].details["dnsZone"], "nacos.local");
    assert_eq!(snapshots[0].details["ttl"], 6);

    assert_eq!(snapshots[1].kind, "forward");
    assert_eq!(snapshots[1].details["upstream"][0], "8.8.8.8:53");
    assert_eq!(snapshots[1].details["maxTtl"], 600);
    assert_eq!(snapshots[1].details["minTtl"], 10);

    assert_eq!(snapshots[2].order, 2);
    assert_eq!(snapshots[2].kind, "native");
    assert_eq!(snapshots[2].details["cacheSize"], 4096);
}

#[test]
fn enabled_protocols_reflect_tls_and_https() {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        server: doggy_dns_basic::ServerConfig,
    }
    fn protos(tls: bool, https: bool) -> Vec<String> {
        let toml_str = format!(
            r#"
[server]
listen_addr = "0.0.0.0"
port = 15353
[server.tls]
enabled = {tls}
cert_path = "/tmp/c.pem"
key_path = "/tmp/c.key"
[server.https]
enabled = {https}
cert_path = "/tmp/c.pem"
key_path = "/tmp/c.key"
"#
        );
        let parsed: Wrapper = toml::from_str(&toml_str).unwrap();
        let config = doggy_dns_basic::DoggyDnsConfig {
            server: parsed.server,
            middleware: Default::default(),
            plugins: Vec::new(),
            remote_config: Default::default(),
            observability: Default::default(),
            xdp: Default::default(),
            mcp: Default::default(),
        };
        McpState::enabled_protocols(&config)
    }

    assert_eq!(protos(false, false), vec!["udp", "tcp"]);
    assert_eq!(protos(true, false), vec!["udp", "tcp", "dot"]);
    assert_eq!(protos(false, true), vec!["udp", "tcp", "doh"]);
    assert_eq!(protos(true, true), vec!["udp", "tcp", "dot", "doh"]);
}
