use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MiddlewareConfig {
    #[serde(default)]
    pub logging: bool,
    #[serde(default)]
    pub log_format: Option<String>,
}
