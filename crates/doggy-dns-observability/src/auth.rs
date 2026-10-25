use anyhow::{Context, Result};
use base64::Engine as _;
use doggy_dns_core::config::ObservabilityConfig;
use reqwest::header::{HeaderName, HeaderValue};
use std::collections::HashMap;

/// Build the OTLP/HTTP headers for every signal.
///
/// `OTEL_EXPORTER_OTLP_HEADERS` provides the baseline; config headers override
/// it. `${ENV_VAR}` is expanded in configured values. `basic_auth` adds an
/// `Authorization` header when no header with that name is already present.
pub(crate) fn resolve_headers(config: &ObservabilityConfig) -> Result<HashMap<String, String>> {
    resolve_with_env(config, |name| std::env::var(name).ok())
}

fn resolve_with_env(
    config: &ObservabilityConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();

    if let Some(raw) = env("OTEL_EXPORTER_OTLP_HEADERS").filter(|value| !value.trim().is_empty()) {
        for pair in raw.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair
                .split_once('=')
                .with_context(|| format!("invalid OTEL_EXPORTER_OTLP_HEADERS entry: {pair}"))?;
            let key = percent_decode(key.trim()).with_context(|| {
                format!("invalid header name in OTEL_EXPORTER_OTLP_HEADERS: {key}")
            })?;
            anyhow::ensure!(
                !key.is_empty(),
                "OTEL_EXPORTER_OTLP_HEADERS contains an empty header name"
            );
            let value = percent_decode(value.trim()).with_context(|| {
                format!("invalid header value in OTEL_EXPORTER_OTLP_HEADERS for {key}")
            })?;
            insert_header(&mut headers, &key, value);
        }
    }

    for (key, value) in &config.headers {
        anyhow::ensure!(
            !key.trim().is_empty(),
            "observability.headers contains an empty header name"
        );
        insert_header(
            &mut headers,
            key.trim(),
            expand_env(value, &env)
                .with_context(|| format!("invalid observability header {key}"))?,
        );
    }

    if let Some(auth) = &config.basic_auth
        && !headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"))
    {
        let username = expand_env(&auth.username, &env)
            .context("invalid observability.basic_auth.username")?;
        let password = expand_env(&auth.password, &env)
            .context("invalid observability.basic_auth.password")?;
        anyhow::ensure!(
            !username.is_empty() && !password.is_empty(),
            "observability.basic_auth username and password must not be empty"
        );
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        headers.insert("Authorization".to_string(), format!("Basic {encoded}"));
    }

    for (name, value) in &headers {
        HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid OTLP header name: {name}"))?;
        HeaderValue::from_str(value)
            .with_context(|| format!("invalid OTLP header value for {name}"))?;
    }

    Ok(headers)
}

fn insert_header(headers: &mut HashMap<String, String>, name: &str, value: String) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name))
        .cloned()
    {
        headers.remove(&existing);
    }
    headers.insert(name.to_string(), value);
}

/// Decode `%XX` escapes from `OTEL_EXPORTER_OTLP_HEADERS` values. The OTLP
/// spec percent-encodes header names and values, including embedded commas.
pub(crate) fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            anyhow::ensure!(
                index + 2 < bytes.len(),
                "truncated percent escape in header value"
            );
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).context("header value is not valid UTF-8 after percent-decoding")
}

fn hex_value(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => anyhow::bail!("invalid hex digit in percent escape"),
    }
}

fn expand_env(value: &str, env: &impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .context("unterminated ${...} environment reference")?;
        let name = &after[..end];
        anyhow::ensure!(
            !name.is_empty(),
            "empty environment reference in observability config"
        );
        let resolved = env(name).with_context(|| {
            format!("environment variable {name} referenced by observability config is not set")
        })?;
        output.push_str(&resolved);
        rest = &after[end + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use doggy_dns_core::config::{BasicAuthConfig, ObservabilityConfig};
    use std::collections::{BTreeMap, HashMap};

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn env_headers_are_overridden_by_config() {
        let mut config = ObservabilityConfig::default();
        config
            .headers
            .insert("x-tenant".to_string(), "config".to_string());

        let headers = resolve_with_env(
            &config,
            env_map(&[
                ("OTEL_EXPORTER_OTLP_HEADERS", "x-tenant=env,x-other=1"),
                ("IGNORED", ""),
            ]),
        )
        .unwrap();

        assert_eq!(headers.get("x-tenant").unwrap(), "config");
        assert_eq!(headers.get("x-other").unwrap(), "1");
    }

    #[test]
    fn env_references_are_expanded() {
        let mut config = ObservabilityConfig::default();
        config
            .headers
            .insert("authorization".to_string(), "Bearer ${TOKEN}".to_string());

        let headers = resolve_with_env(&config, env_map(&[("TOKEN", "secret")])).unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
    }

    #[test]
    fn missing_env_reference_is_an_error() {
        let mut config = ObservabilityConfig::default();
        config
            .headers
            .insert("authorization".to_string(), "Bearer ${MISSING}".to_string());

        let error = resolve_with_env(&config, env_map(&[])).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("MISSING")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn basic_auth_adds_authorization_header() {
        let config = ObservabilityConfig {
            basic_auth: Some(BasicAuthConfig {
                username: "root@example.com".to_string(),
                password: "secret".to_string(),
            }),
            ..ObservabilityConfig::default()
        };

        let headers = resolve_with_env(&config, env_map(&[])).unwrap();
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("root@example.com:secret")
        );
        assert_eq!(headers.get("Authorization").unwrap(), &expected);
    }

    #[test]
    fn explicit_authorization_wins_over_basic_auth() {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_string(), "Bearer token".to_string());
        let config = ObservabilityConfig {
            headers,
            basic_auth: Some(BasicAuthConfig {
                username: "user".to_string(),
                password: "pass".to_string(),
            }),
            ..ObservabilityConfig::default()
        };

        let headers = resolve_with_env(&config, env_map(&[])).unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer token");
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn basic_auth_supports_env_expansion() {
        let config = ObservabilityConfig {
            basic_auth: Some(BasicAuthConfig {
                username: "${USER}".to_string(),
                password: "${PASS}".to_string(),
            }),
            ..ObservabilityConfig::default()
        };

        let headers = resolve_with_env(&config, env_map(&[("USER", "u"), ("PASS", "p")])).unwrap();
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u:p")
        );
        assert_eq!(headers.get("Authorization").unwrap(), &expected);
    }

    #[test]
    fn env_header_values_are_percent_decoded() {
        let config = ObservabilityConfig::default();
        let headers = resolve_with_env(
            &config,
            env_map(&[(
                "OTEL_EXPORTER_OTLP_HEADERS",
                "Authorization=Basic%20cm9vdDpzZWNyZXQ%3D,stream-name=doggy%2Ddns",
            )]),
        )
        .unwrap();

        assert_eq!(
            headers.get("Authorization").unwrap(),
            "Basic cm9vdDpzZWNyZXQ="
        );
        assert_eq!(headers.get("stream-name").unwrap(), "doggy-dns");
    }

    #[test]
    fn invalid_percent_escapes_are_rejected() {
        let config = ObservabilityConfig::default();
        let error = resolve_with_env(
            &config,
            env_map(&[("OTEL_EXPORTER_OTLP_HEADERS", "x-test=bad%2")]),
        )
        .unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("percent")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn malformed_env_headers_are_rejected() {
        let config = ObservabilityConfig::default();
        let error = resolve_with_env(&config, env_map(&[("OTEL_EXPORTER_OTLP_HEADERS", "bad")]))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid OTEL_EXPORTER_OTLP_HEADERS")
        );
    }

    #[test]
    fn invalid_header_names_are_rejected() {
        let mut config = ObservabilityConfig::default();
        config
            .headers
            .insert("bad header".to_string(), "value".to_string());

        let error = resolve_with_env(&config, env_map(&[])).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("header name")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn invalid_header_values_are_rejected() {
        let mut config = ObservabilityConfig::default();
        config
            .headers
            .insert("x-test".to_string(), "line\nbreak".to_string());

        let error = resolve_with_env(&config, env_map(&[])).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("header value")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn config_header_overrides_env_header_case_insensitively() {
        let mut config = ObservabilityConfig::default();
        config
            .headers
            .insert("x-tenant".to_string(), "config".to_string());

        let headers = resolve_with_env(
            &config,
            env_map(&[("OTEL_EXPORTER_OTLP_HEADERS", "X-Tenant=env")]),
        )
        .unwrap();

        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("x-tenant").unwrap(), "config");
    }

    #[test]
    fn empty_config_headers_are_ignored() {
        let config = ObservabilityConfig {
            headers: BTreeMap::new(),
            ..ObservabilityConfig::default()
        };
        let headers = resolve_with_env(&config, env_map(&[])).unwrap();
        assert!(headers.is_empty());
    }
}
