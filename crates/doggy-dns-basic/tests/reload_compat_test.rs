#![allow(clippy::disallowed_methods)]

//! Which config differences a running process may absorb, and which it must
//! refuse.
//!
//! A reload rebuilds the query path only, so the compat check is an allow-list:
//! `[[plugins]]`, `server.rotate_answers` and `middleware.logging` may differ,
//! anything else makes the whole reload invalid. Each forbidden path gets its own
//! case here because the message an operator reads has to name the line they
//! actually changed.

use doggy_dns_basic::{DoggyDnsConfig, is_reloadable, parse_config, reload_conflicts};

/// A config that passes validation and touches every compared section, so each
/// case below changes exactly one thing.
const BASE: &str = r#"
[server]
listen_addr = "0.0.0.0"
port = 53
tcp_timeout = 10
shutdown_timeout = 5
worker_threads = 2
reuseport_listen_num = 2
rotate_answers = true
cpu_affinity = [0, 1]

[server.tls]
enabled = false
port = 853
cert_path = "/certs/cert.pem"
key_path = "/certs/key.pem"

[server.https]
enabled = false
port = 443
cert_path = "/certs/cert.pem"
key_path = "/certs/key.pem"

[middleware]
logging = true
log_format = "json"

[[plugins]]
kind = "nacos"
enabled = true
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DEFAULT_GROUP"
dns_zone = "nacos.local"
ttl = 6

[[plugins]]
kind = "forward"
enabled = true
upstream = ["8.8.8.8:53"]

[remote_config]
enabled = false
server_addr = "127.0.0.1:8848"
namespace = "public"
group = "DOGGY_DNS"
data_id = "doggy-dns.toml"

[observability]
enabled = false
endpoint = "http://127.0.0.1:4318"

[xdp]
enabled = false
interface = "eth0"

[control]
enabled = false
listen_addr = "127.0.0.1"
port = 8787
endpoints = ["healthz"]
"#;

fn base() -> DoggyDnsConfig {
    parse_config(BASE).expect("the base config must be valid for these cases")
}

type Mutation = fn(&mut DoggyDnsConfig);

/// Every change that must be refused, paired with the path it reports.
const FORBIDDEN: &[(&str, Mutation)] = &[
    ("server.listen_addr", |config| {
        config.server.listen_addr = "127.0.0.1".to_string()
    }),
    ("server.port", |config| config.server.port = 5353),
    ("server.tcp_timeout", |config| {
        config.server.tcp_timeout = 20
    }),
    ("server.shutdown_timeout", |config| {
        config.server.shutdown_timeout = 30
    }),
    ("server.worker_threads", |config| {
        config.server.worker_threads = 4
    }),
    ("server.reuseport_listen_num", |config| {
        config.server.reuseport_listen_num = 4
    }),
    ("server.cpu_affinity", |config| {
        config.server.cpu_affinity = Some(vec![2, 3])
    }),
    ("server.tls", |config| {
        config
            .server
            .tls
            .as_mut()
            .expect("the base config configures tls")
            .port = 8883;
    }),
    ("server.https", |config| {
        config
            .server
            .https
            .as_mut()
            .expect("the base config configures https")
            .enabled = true;
    }),
    ("middleware.log_format", |config| {
        config.middleware.log_format = Some("text".to_string())
    }),
    ("remote_config", |config| {
        config.remote_config.data_id = "other.toml".to_string()
    }),
    ("observability", |config| {
        config.observability.enabled = true
    }),
    ("xdp", |config| config.xdp.enabled = true),
    ("control", |config| config.control.enabled = true),
];

#[test]
fn an_unchanged_config_is_reloadable() {
    assert!(reload_conflicts(&base(), &base()).is_empty());
    assert!(is_reloadable(&base(), &base()));
}

#[test]
fn each_forbidden_change_reports_its_own_path() {
    for (path, mutate) in FORBIDDEN {
        let mut candidate = base();
        mutate(&mut candidate);
        assert_eq!(
            reload_conflicts(&base(), &candidate),
            vec![(*path).to_string()],
            "changing {path} must be refused by that exact path"
        );
        assert!(
            !is_reloadable(&base(), &candidate),
            "{path} must not be reloadable"
        );
    }
}

#[test]
fn every_forbidden_change_is_refused_together() {
    // Operators edit several lines at once; one reload request must list them
    // all instead of accepting a config that is only half checked.
    let mut candidate = base();
    for (_, mutate) in FORBIDDEN {
        mutate(&mut candidate);
    }
    let conflicts = reload_conflicts(&base(), &candidate);
    let expected: Vec<String> = FORBIDDEN
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect();
    assert_eq!(conflicts, expected, "conflicts are reported in table order");
}

#[test]
fn query_path_changes_are_reloadable() {
    let reloadable: [(&str, Mutation); 5] = [
        ("plugin ttl", |config| config.plugins[0].ttl = 60),
        ("plugin zone", |config| {
            config.plugins[0].dns_zone = Some("internal.local".to_string())
        }),
        ("plugin upstream", |config| {
            config.plugins[1].upstream = Some(vec!["1.1.1.1:53".to_string()])
        }),
        ("rotation", |config| config.server.rotate_answers = false),
        ("logging", |config| config.middleware.logging = false),
    ];
    for (label, mutate) in reloadable {
        let mut candidate = base();
        mutate(&mut candidate);
        assert!(
            is_reloadable(&base(), &candidate),
            "changing {label} must be applied in place, got {:?}",
            reload_conflicts(&base(), &candidate)
        );
    }
}

#[test]
fn disabling_a_plugin_is_reloadable() {
    // The chain is rebuilt from the enabled plugins, so dropping one is a
    // query-path change like any other.
    let mut candidate = base();
    candidate.plugins[0].enabled = false;
    assert!(is_reloadable(&base(), &candidate));
}

#[test]
fn a_forbidden_change_wins_over_a_reloadable_one() {
    // A push that both rotates the zone and moves the port is refused whole:
    // applying the plugin half would leave the process serving on a port the
    // operator believes has changed.
    let mut candidate = base();
    candidate.plugins[0].ttl = 60;
    candidate.server.port = 5353;
    assert_eq!(
        reload_conflicts(&base(), &candidate),
        vec!["server.port".to_string()],
        "only the forbidden difference may be reported"
    );
}

#[test]
fn plugin_order_changes_are_reloadable() {
    let mut candidate = base();
    candidate.plugins.reverse();
    assert!(is_reloadable(&base(), &candidate));
}
