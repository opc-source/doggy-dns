use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MiddlewareConfig {
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub log_format: Option<String>,
}
