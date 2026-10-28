#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::env::{expand_env, expand_env_in_value};
use std::collections::HashMap;

fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    let map: HashMap<&str, &str> = pairs.iter().copied().collect();
    move |name| map.get(name).map(|value| (*value).to_string())
}

#[test]
fn expands_every_reference_in_a_string() {
    let expanded = expand_env("${A}-${B}-${A}", &lookup(&[("A", "1"), ("B", "2")])).unwrap();
    assert_eq!(expanded, "1-2-1");
}

#[test]
fn strings_without_references_are_unchanged() {
    let expanded = expand_env("plain value", &lookup(&[])).unwrap();
    assert_eq!(expanded, "plain value");
}

#[test]
fn unterminated_reference_is_rejected() {
    let error = expand_env("${FOO", &lookup(&[("FOO", "x")])).unwrap_err();
    assert!(
        error.to_string().contains("unterminated"),
        "unexpected error: {error}"
    );
}

#[test]
fn empty_reference_is_rejected() {
    let error = expand_env("${}", &lookup(&[])).unwrap_err();
    assert!(
        error.to_string().contains("empty environment reference"),
        "unexpected error: {error}"
    );
}

#[test]
fn missing_variable_is_rejected() {
    let error = expand_env("${MISSING}", &lookup(&[])).unwrap_err();
    assert!(
        error.to_string().contains("MISSING"),
        "unexpected error: {error}"
    );
}

#[test]
fn expands_strings_in_tables_and_arrays_of_tables() {
    let mut doc: toml::Value = toml::from_str(
        r#"
[observability]
basic_auth = { username = "root", password = "${PASS}" }
headers = { authorization = "Bearer ${TOKEN}" }

[[plugins]]
kind = "nacos"
server_addr = "${NACOS_ADDR}"
"#,
    )
    .unwrap();

    expand_env_in_value(
        &mut doc,
        &lookup(&[
            ("PASS", "s3cret"),
            ("TOKEN", "abc"),
            ("NACOS_ADDR", "127.0.0.1:8848"),
        ]),
    )
    .unwrap();

    assert_eq!(
        doc["observability"]["basic_auth"]["password"].as_str(),
        Some("s3cret")
    );
    assert_eq!(
        doc["observability"]["headers"]["authorization"].as_str(),
        Some("Bearer abc")
    );
    assert_eq!(
        doc["plugins"][0]["server_addr"].as_str(),
        Some("127.0.0.1:8848")
    );
}

#[test]
fn leaves_non_string_values_untouched() {
    let mut doc: toml::Value = toml::from_str(
        r#"
[server]
port = 5353
worker_threads = 2

[observability]
enabled = true
sample_ratio = 0.5
"#,
    )
    .unwrap();
    let before = doc.clone();

    expand_env_in_value(&mut doc, &lookup(&[])).unwrap();
    assert_eq!(doc, before);
}

#[test]
fn missing_variable_reports_the_config_path() {
    let mut doc: toml::Value = toml::from_str(
        r#"
[observability.basic_auth]
username = "root"
password = "${MISSING}"
"#,
    )
    .unwrap();

    let error = expand_env_in_value(&mut doc, &lookup(&[])).unwrap_err();
    let chain = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(" / ");
    assert!(
        chain.contains("MISSING") && chain.contains("basic_auth.password"),
        "unexpected error chain: {chain}"
    );
}
