use thiserror::Error;

#[derive(Debug, Error)]
pub enum RedactError {
    #[error("failed to parse TOML configuration")]
    ParseError(#[from] toml::de::Error),
    #[error("failed to serialise redacted configuration")]
    SerializeError(#[from] toml::ser::Error),
}

const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "authorization",
    "api_key",
    "apikey",
];

fn is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|s| lower == *s)
}

/// Parse TOML, replace sensitive values with `"***"`, and return the
/// serialised result together with the list of redacted key paths.
pub fn redact_toml(input: &str) -> Result<(String, Vec<String>), RedactError> {
    let mut doc: toml::Value = toml::from_str(input)?;
    let mut redacted = Vec::new();
    doggy_dns_basic::walk::walk_strings(&mut doc, "", &mut |path, key, text| {
        if key.is_some_and(is_sensitive) {
            *text = "***".to_string();
            redacted.push(path.to_string());
        }
        Ok(())
    })
    .expect("the redaction visitor never fails");
    let output = toml::to_string_pretty(&doc)?;
    Ok((output, redacted))
}
