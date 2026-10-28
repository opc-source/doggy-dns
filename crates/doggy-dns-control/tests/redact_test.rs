#![allow(clippy::disallowed_methods)]

use doggy_dns_control::redact::redact_toml;

#[test]
fn redacts_nested_secrets_but_keeps_stream_name() {
    let input = r#"
[observability.basic_auth]
username = "root@example.com"
password = "hunter2"

[observability.headers]
stream-name = "doggy-dns"
authorization = "Basic abc123"
"#;
    let (out, redacted) = redact_toml(input).unwrap();
    assert!(out.contains("root@example.com"));
    assert!(out.contains("stream-name"));
    assert!(!out.contains("hunter2"));
    assert!(!out.contains("abc123"));
    assert!(redacted.iter().any(|k| k.ends_with(".password")));
    assert!(redacted.iter().any(|k| k.ends_with(".authorization")));
}

#[test]
fn invalid_toml_returns_error_not_raw_input() {
    let err = redact_toml("not = [valid").unwrap_err();
    assert!(!format!("{err}").contains("not = [valid"));
}

#[test]
fn redacts_token_in_top_level_table() {
    let input = r#"
[mcp]
enabled = true
token = "super-secret-token"
"#;
    let (out, redacted) = redact_toml(input).unwrap();
    assert!(!out.contains("super-secret-token"));
    assert!(redacted.iter().any(|k| k.ends_with(".token")));
}

#[test]
fn redacts_api_key_and_passwd() {
    let input = r#"
[service]
api_key = "key-12345"
passwd = "mypass"
name = "visible"
"#;
    let (out, redacted) = redact_toml(input).unwrap();
    assert!(!out.contains("key-12345"));
    assert!(!out.contains("mypass"));
    assert!(out.contains("visible"));
    assert!(redacted.iter().any(|k| k.ends_with(".api_key")));
    assert!(redacted.iter().any(|k| k.ends_with(".passwd")));
}

#[test]
fn redacts_secrets_inside_arrays() {
    let input = r#"
[[endpoints]]
name = "primary"
secret = "array-secret"

[[endpoints]]
name = "secondary"
token = "array-token"
"#;
    let (out, redacted) = redact_toml(input).unwrap();
    assert!(!out.contains("array-secret"));
    assert!(!out.contains("array-token"));
    assert!(out.contains("primary"));
    assert!(out.contains("secondary"));
    assert!(redacted.len() >= 2);
}

#[test]
fn case_insensitive_key_matching() {
    let input = r#"
[auth]
Password = "mixed-case"
API_KEY = "upper-key"
"#;
    let (out, redacted) = redact_toml(input).unwrap();
    assert!(!out.contains("mixed-case"));
    assert!(!out.contains("upper-key"));
    assert!(redacted.len() >= 2);
}

#[test]
fn no_secrets_returns_empty_redacted_keys() {
    let input = r#"
[server]
listen_addr = "127.0.0.1"
port = 53
"#;
    let (out, redacted) = redact_toml(input).unwrap();
    assert!(out.contains("127.0.0.1"));
    assert!(redacted.is_empty());
}
