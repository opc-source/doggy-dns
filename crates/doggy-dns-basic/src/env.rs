//! `${ENV_VAR}` substitution for string values in the parsed configuration.
//!
//! Expansion runs on the parsed TOML document before it is deserialised, so
//! every string field — plugin addresses, TLS paths, MCP tokens, observability
//! credentials — can reference the environment. Keys and non-string values are
//! left untouched: a numeric field cannot be provided via `${ENV_VAR}` because
//! the substitution happens at the string level.
//!
//! Missing or malformed references are fatal so a typo cannot silently fall
//! back to an empty value.

use anyhow::{Context, Result};

/// Expand every `${NAME}` reference in `value` using `lookup`.
///
/// `lookup` returns `None` when the variable is not set, which is an error.
/// References cannot be nested: the first `}` terminates the name.
pub fn expand_env(value: &str, lookup: &impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .context("unterminated ${...} environment reference")?;
        let name = &after[..end];
        anyhow::ensure!(!name.is_empty(), "empty environment reference");
        let resolved =
            lookup(name).with_context(|| format!("environment variable {name} is not set"))?;
        output.push_str(&resolved);
        rest = &after[end + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

/// Expand `${NAME}` references in every string value of a parsed TOML document.
///
/// Table keys are not expanded; array elements and nested tables are visited
/// recursively. Errors carry the dotted path of the offending value, e.g.
/// `observability.basic_auth.password`.
pub fn expand_env_in_value(
    value: &mut toml::Value,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<()> {
    expand_value(value, "", lookup)
}

fn expand_value(
    value: &mut toml::Value,
    path: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<()> {
    match value {
        toml::Value::String(text) => {
            let expanded = expand_env(text, lookup)
                .with_context(|| format!("in configuration value {path}"))?;
            *text = expanded;
        }
        toml::Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                expand_value(child, &child_path, lookup)?;
            }
        }
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                expand_value(item, &format!("{path}[{index}]"), lookup)?;
            }
        }
        _ => {}
    }
    Ok(())
}
