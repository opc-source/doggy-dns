//! Load and validate doggy-dns configuration from a TOML file.

use crate::DoggyDnsConfig;

/// Load and validate config from a TOML file path.
pub fn load_config(path: &str) -> anyhow::Result<DoggyDnsConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: DoggyDnsConfig = toml::from_str(&content)?;
    crate::validate::validate_config(&config)?;
    Ok(config)
}
