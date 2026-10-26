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

fn redact_value(value: &mut toml::Value, path: &str, redacted: &mut Vec<String>) {
    match value {
        toml::Value::Table(table) => {
            let keys: Vec<String> = table.keys().cloned().collect();
            for key in keys {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if let Some(child) = table.get_mut(&key) {
                    if is_sensitive(&key) {
                        if let toml::Value::String(s) = child {
                            *s = "***".to_string();
                            redacted.push(child_path);
                        } else {
                            redact_value(child, &child_path, redacted);
                        }
                    } else {
                        redact_value(child, &child_path, redacted);
                    }
                }
            }
        }
        toml::Value::Array(arr) => {
            for (i, item) in arr.iter_mut().enumerate() {
                let child_path = format!("{path}[{i}]");
                redact_value(item, &child_path, redacted);
            }
        }
        _ => {}
    }
}

/// Parse TOML, replace sensitive values with `"***"`, and return the
/// serialised result together with the list of redacted key paths.
pub fn redact_toml(input: &str) -> Result<(String, Vec<String>), RedactError> {
    let mut doc: toml::Value = toml::from_str(input)?;
    let mut redacted = Vec::new();
    redact_value(&mut doc, "", &mut redacted);
    let output = toml::to_string_pretty(&doc)?;
    Ok((output, redacted))
}
