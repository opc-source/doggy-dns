//! Node-side service control plane: one HTTP listener whose endpoints are
//! selected by configuration — the read-only MCP introspection protocol at
//! `/mcp`, plus unauthenticated `healthz`/`readyz` probes backed by live
//! runtime signals.

pub mod live;
pub mod redact;
pub mod server;
pub mod state;
pub mod transport;

pub use transport::{McpServerHandle, start};

use serde::{Deserialize, Serialize};

use doggy_dns_basic::{PluginConfig, PluginKind};

pub use doggy_dns_basic::ControlConfig;

/// Snapshot of a single plugin for the MCP `list_plugins` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSnapshot {
    pub order: usize,
    pub kind: String,
    pub enabled: bool,
    pub details: serde_json::Value,
}

/// Redacted config captured once at startup, or the error that prevented it.
#[derive(Debug, Clone)]
pub enum ConfigSnapshot {
    Redacted {
        content: String,
        redacted_keys: Vec<String>,
    },
    Error {
        code: &'static str,
        message: String,
    },
}

impl ConfigSnapshot {
    /// Redact raw TOML content once. Never touches the filesystem: the
    /// content comes from `doggy_dns_basic::load_config`.
    pub fn capture(raw: &str) -> Self {
        match redact::redact_toml(raw) {
            Ok((content, redacted_keys)) => Self::Redacted {
                content,
                redacted_keys,
            },
            Err(e) => Self::Error {
                code: "config_parse_error",
                message: format!("failed to parse config: {e}"),
            },
        }
    }
}

/// Build tool-ready snapshots from the configured plugin chain, preserving
/// evaluation order and applying per-kind display defaults.
pub fn snapshots_from(plugins: &[PluginConfig]) -> Vec<PluginSnapshot> {
    plugins
        .iter()
        .enumerate()
        .map(|(order, p)| PluginSnapshot::from_config(order, p))
        .collect()
}

impl PluginSnapshot {
    pub fn from_config(order: usize, p: &PluginConfig) -> Self {
        use serde_json::Value;

        fn str_val(s: &str) -> Value {
            Value::String(s.to_string())
        }

        let kind = format!("{:?}", p.kind).to_ascii_lowercase();
        let details = match p.kind {
            PluginKind::Nacos => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "serverAddr".into(),
                    str_val(p.server_addr.as_deref().unwrap_or("127.0.0.1:8848")),
                );
                map.insert(
                    "namespace".into(),
                    str_val(p.namespace.as_deref().unwrap_or("public")),
                );
                map.insert(
                    "group".into(),
                    str_val(p.group.as_deref().unwrap_or("DEFAULT_GROUP")),
                );
                map.insert(
                    "dnsZone".into(),
                    str_val(p.dns_zone.as_deref().unwrap_or("nacos.local")),
                );
                map.insert("ttl".into(), Value::from(p.ttl));
                Value::Object(map)
            }
            PluginKind::Native => {
                let mut map = serde_json::Map::new();
                map.insert("cacheSize".into(), Value::from(p.cache_size));
                map.insert("minTtl".into(), Value::from(p.min_ttl));
                map.insert("maxTtl".into(), Value::from(p.max_ttl));
                Value::Object(map)
            }
            PluginKind::Forward => {
                let mut map = serde_json::Map::new();
                let upstream: Vec<Value> = p
                    .upstream
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|s| str_val(s))
                    .collect();
                map.insert("upstream".into(), Value::Array(upstream));
                map.insert("cacheSize".into(), Value::from(p.cache_size));
                map.insert("minTtl".into(), Value::from(p.min_ttl));
                map.insert("maxTtl".into(), Value::from(p.max_ttl));
                Value::Object(map)
            }
        };
        Self {
            order,
            kind,
            enabled: p.enabled,
            details,
        }
    }
}

/// Build an `McpState` from the individual parts.
/// Takes `pid` and `started_at` from the current process.
pub fn build_state(
    host: String,
    version: String,
    listen_addr: String,
    protocols: Vec<String>,
    plugins: Vec<PluginSnapshot>,
    config_content: &str,
) -> state::McpState {
    state::McpState::from_parts(
        host,
        std::process::id(),
        version,
        chrono::Utc::now(),
        listen_addr,
        protocols,
        plugins,
        config_content,
    )
}
