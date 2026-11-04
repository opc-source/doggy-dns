#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use doggy_dns_basic::PluginConfig;
use doggy_dns_control::state::McpState;

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
fn snapshots_report_configured_fields_only_and_order() {
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
    let snapshots = doggy_dns_control::snapshots_from(&parsed.plugins);

    assert_eq!(snapshots.len(), 3);
    assert_eq!(snapshots[0].order, 0);
    assert_eq!(snapshots[0].kind, "nacos");
    assert!(!snapshots[0].enabled);
    assert!(
        snapshots[0].details.get("serverAddr").is_none(),
        "an entry that configured no server reports no server, not a default nobody wrote"
    );
    assert!(
        snapshots[0].details.get("dnsZone").is_none(),
        "the same for the zone: {:?}",
        snapshots[0].details
    );
    assert_eq!(snapshots[0].details["ttl"], 6);

    assert_eq!(snapshots[1].kind, "forward");
    assert_eq!(snapshots[1].details["upstream"][0], "8.8.8.8:53");
    assert_eq!(snapshots[1].details["maxTtl"], 600);
    assert_eq!(snapshots[1].details["minTtl"], 10);

    assert_eq!(snapshots[2].order, 2);
    assert_eq!(snapshots[2].kind, "native");
    assert_eq!(snapshots[2].details["cacheSize"], 4096);
}

/// A config reload replaces the runtime snapshot whole: the MCP tools must
/// then report what is in force, not what the process started with — and the
/// freshly pushed content must go through redaction like the startup file.
#[test]
fn update_runtime_replaces_config_and_plugins() {
    let startup = r#"
[observability.basic_auth]
username = "root@example.com"
password = "startup-secret"

[[plugins]]
kind = "native"
"#;
    let pushed = r#"
[observability.basic_auth]
username = "root@example.com"
password = "pushed-secret"

[[plugins]]
kind = "forward"
upstream = ["1.1.1.1:53"]
"#;
    let state = McpState::from_parts(
        "dns-node-1".into(),
        1,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        doggy_dns_control::snapshots_from(&plugins_of(startup)),
        startup,
    );

    {
        let runtime = state.runtime();
        let doggy_dns_control::ConfigSnapshot::Redacted { content, .. } = &runtime.config else {
            panic!("startup content is valid TOML");
        };
        assert!(!content.contains("startup-secret"), "redacted at startup");
        assert_eq!(runtime.plugins.len(), 1);
        assert_eq!(runtime.plugins[0].kind, "native");
    }

    state.update_runtime(pushed, &plugins_of(pushed));

    let runtime = state.runtime();
    let doggy_dns_control::ConfigSnapshot::Redacted {
        content,
        redacted_keys,
    } = &runtime.config
    else {
        panic!("pushed content is valid TOML");
    };
    assert!(!content.contains("pushed-secret"), "redacted after reload");
    assert!(!content.contains("startup-secret"), "old content is gone");
    assert!(redacted_keys.iter().any(|k| k.ends_with(".password")));
    assert_eq!(runtime.plugins.len(), 1);
    assert_eq!(runtime.plugins[0].kind, "forward");
    assert_eq!(runtime.plugins[0].details["upstream"][0], "1.1.1.1:53");
}

fn plugins_of(content: &str) -> Vec<PluginConfig> {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        plugins: Vec<PluginConfig>,
    }
    toml::from_str::<Wrapper>(content).unwrap().plugins
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
            control: Default::default(),
        };
        McpState::enabled_protocols(&config)
    }

    assert_eq!(protos(false, false), vec!["udp", "tcp"]);
    assert_eq!(protos(true, false), vec!["udp", "tcp", "dot"]);
    assert_eq!(protos(false, true), vec!["udp", "tcp", "doh"]);
    assert_eq!(protos(true, true), vec!["udp", "tcp", "dot", "doh"]);
}
