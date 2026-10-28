//! Load and validate doggy-dns configuration from a TOML file.

use crate::DoggyDnsConfig;

/// Load and validate config from a TOML file path.
///
/// String values may reference the environment as `${NAME}`; references are
/// expanded before deserialisation and a missing variable aborts startup.
///
/// Returns the parsed config together with the raw file content, so
/// consumers (e.g. the MCP `get_config` tool) never need to re-read the
/// file from disk. The raw content is returned *unexpanded*, keeping secrets
/// out of the MCP response.
pub fn load_config(path: &str) -> anyhow::Result<(DoggyDnsConfig, String)> {
    let content = std::fs::read_to_string(path)?;
    let config = parse_config(&content)?;
    Ok((config, content))
}

/// Parse, expand `${ENV_VAR}` references, and validate TOML content.
fn parse_config(content: &str) -> anyhow::Result<DoggyDnsConfig> {
    let mut document: toml::Value = toml::from_str(content)?;
    crate::env::expand_env_in_value(&mut document, &|name| std::env::var(name).ok())?;
    let config: DoggyDnsConfig = document.try_into()?;
    crate::validate::validate_config(&config)?;
    Ok(config)
}
