use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub server_addr: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub data_id: String,
}
