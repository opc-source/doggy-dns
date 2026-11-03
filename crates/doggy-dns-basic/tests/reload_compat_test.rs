#![allow(clippy::disallowed_methods)]

//! Which config differences a running process may absorb, and which it must
//! refuse.
//!
//! A reload rebuilds the query path only, so the compat check is an allow-list:
//! `[[plugins]]`, `server.rotate_answers` and `middleware.logging` may differ,
//! anything else makes the whole reload invalid. The list is enforced by walking
//! the two serialised configs, so the cases below check two things: that each
//! forbidden change reports the exact line an operator edited, and that every
//! leaf of a config is covered by the walk — the property that makes a newly
//! added field restart-required without anyone having to list it.

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
    ("server.tls.port", |config| {
        config
            .server
            .tls
            .as_mut()
            .expect("the base config configures tls")
            .port = 8883;
    }),
    ("server.https.enabled", |config| {
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
    ("remote_config.data_id", |config| {
        config.remote_config.data_id = "other.toml".to_string()
    }),
    ("observability.enabled", |config| {
        config.observability.enabled = true
    }),
    ("xdp.enabled", |config| config.xdp.enabled = true),
    ("control.enabled", |config| config.control.enabled = true),
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
    let mut conflicts = reload_conflicts(&base(), &candidate);
    let mut expected: Vec<String> = FORBIDDEN
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect();
    // The walk reports in document order (each table's keys sorted), which is
    // not the order these cases are written in.
    conflicts.sort();
    expected.sort();
    assert_eq!(conflicts, expected);
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

/// The whole config as the walk sees it.
fn as_document(config: &DoggyDnsConfig) -> toml::Value {
    toml::Value::try_from(config).expect("every config must serialise to TOML to be comparable")
}

/// Every leaf path of a document, using the walk's convention: a table is
/// descended, anything else — arrays included — is one leaf.
fn leaf_paths(prefix: &str, document: &toml::Value, out: &mut Vec<String>) {
    match document {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                leaf_paths(&path, value, out);
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
        }
    }
}

fn leaves_of(document: &toml::Value) -> Vec<String> {
    let mut leaves = Vec::new();
    leaf_paths("", document, &mut leaves);
    leaves.sort();
    leaves
}

/// Change a leaf without changing what kind of value it is.
fn mutate(value: &mut toml::Value) {
    match value {
        toml::Value::String(text) => text.push_str("-mutated"),
        toml::Value::Integer(number) => *number = if *number == 0 { 1 } else { *number - 1 },
        toml::Value::Boolean(flag) => *flag = !*flag,
        toml::Value::Float(number) => *number += 0.5,
        toml::Value::Array(items) => {
            match items.first() {
                // Repeating an element changes the array without changing the
                // type of anything in it.
                Some(first) => items.push(first.clone()),
                // Every empty array in the config holds strings today; another
                // element type needs a case here.
                None => items.push(toml::Value::String("mutated".to_string())),
            }
        }
        // A table is descended to its own leaves, so one never reaches here.
        toml::Value::Table(_) => unreachable!("a table is not a leaf"),
        toml::Value::Datetime(_) => {
            // No config field is a TOML date today; if one appears, this test
            // has to learn how to change one.
            unreachable!("the config has a datetime leaf; teach mutate() to change it")
        }
    }
}

/// The value at a dotted leaf path.
fn lookup<'a>(document: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    let mut cursor = document;
    for key in path.split('.') {
        cursor = cursor.get(key)?;
    }
    Some(cursor)
}

/// Apply `mutate` to the leaf at `path` of `document`.
fn mutate_leaf(document: &mut toml::Value, path: &str) {
    let mut cursor = document;
    for key in path.split('.') {
        cursor = cursor
            .get_mut(key)
            .unwrap_or_else(|| panic!("{path} names a leaf that the document does not have"));
    }
    mutate(cursor);
}

fn is_hot(path: &str) -> bool {
    doggy_dns_basic::reload::HOT_PATHS
        .iter()
        .any(|hot| path == *hot || path.starts_with(&format!("{hot}.")))
}

#[test]
fn the_serialised_config_round_trips_without_losing_a_field() {
    // The structural check is only as exhaustive as this round trip: a field
    // that does not survive it is invisible to the gate.
    let in_force = base();
    let back: DoggyDnsConfig = as_document(&in_force)
        .try_into()
        .expect("a serialised config must deserialise back to itself");
    assert_eq!(in_force, back);
}

#[test]
fn the_walk_reports_every_leaf_of_the_config() {
    let in_force = as_document(&base());
    let leaves = leaves_of(&in_force);
    assert!(
        !leaves.is_empty(),
        "the base config must exercise every section"
    );
    for path in leaves {
        let mut candidate = in_force.clone();
        mutate_leaf(&mut candidate, &path);
        assert_eq!(
            doggy_dns_basic::reload::diff_paths(&in_force, &candidate),
            vec![path.clone()],
            "changing {path} alone must be reported at {path}"
        );
    }
}

#[test]
fn the_gate_refuses_every_leaf_outside_the_hot_paths() {
    let in_force = base();
    let document = as_document(&in_force);
    let mut tag_valued = Vec::new();
    for path in leaves_of(&document) {
        let mut candidate_document = document.clone();
        mutate_leaf(&mut candidate_document, &path);
        // A leaf whose Rust type accepts only a fixed set of tags (the
        // `#[serde(rename)]` enums) cannot be mutated into another valid tag
        // without knowing that set, so it does not deserialise. The walk case
        // above already proves those paths are compared; here they are checked
        // to be exactly the tag-valued ones and nothing else.
        let Ok(candidate) = candidate_document.try_into::<DoggyDnsConfig>() else {
            assert!(
                matches!(lookup(&document, &path), Some(toml::Value::String(_))),
                "{path} is not a string tag, so the generic mutation should have \
                 produced a value that deserialises"
            );
            tag_valued.push(path);
            continue;
        };
        let conflicts = reload_conflicts(&in_force, &candidate);
        if is_hot(&path) {
            assert!(
                conflicts.is_empty(),
                "{path} is on the hot list and must be applied in place, got {conflicts:?}"
            );
        } else {
            assert_eq!(
                conflicts,
                vec![path.clone()],
                "{path} is not on the hot list and must be refused by that path"
            );
        }
    }
    // Not a list to maintain: a reload that could not be checked this way has to
    // be explained.
    assert!(
        tag_valued.iter().all(|path| !is_hot(path)),
        "a hot path must accept any value its type allows: {tag_valued:?}"
    );
}
