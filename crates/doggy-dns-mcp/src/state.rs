use chrono::{DateTime, Utc};
use std::path::PathBuf;

use crate::PluginSnapshot;

/// Immutable snapshot of a doggy-dns instance's state, built once at startup.
#[derive(Debug, Clone)]
pub struct McpState {
    pub instance_id: String,
    pub host: String,
    pub pid: u32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub listen_addr: String,
    pub protocols: Vec<String>,
    pub plugins: Vec<PluginSnapshot>,
    pub config_path: PathBuf,
}

impl McpState {
    pub fn from_parts(
        host: String,
        pid: u32,
        version: String,
        started_at: DateTime<Utc>,
        listen_addr: String,
        protocols: Vec<String>,
        plugins: Vec<PluginSnapshot>,
        config_path: PathBuf,
    ) -> Self {
        let instance_id = format!("{host}-{pid}");
        Self {
            instance_id,
            host,
            pid,
            version,
            started_at,
            listen_addr,
            protocols,
            plugins,
            config_path,
        }
    }
}
