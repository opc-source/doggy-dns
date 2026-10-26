//! Load and validate doggy-dns configuration from a TOML file.

use crate::DoggyDnsConfig;

/// Load and validate config from a TOML file path.
///
/// Returns the parsed config together with the raw file content, so
/// consumers (e.g. the MCP `get_config` tool) never need to re-read the
/// file from disk.
pub fn load_config(path: &str) -> anyhow::Result<(DoggyDnsConfig, String)> {
    let content = std::fs::read_to_string(path)?;
    let config: DoggyDnsConfig = toml::from_str(&content)?;
    crate::validate::validate_config(&config)?;
    Ok((config, content))
}
