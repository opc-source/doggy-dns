//! Node-side service control plane: one HTTP listener whose endpoints are
//! selected by configuration — the read-only MCP introspection protocol at
//! `/mcp`, plus unauthenticated `healthz`/`readyz` probes backed by live
//! runtime signals.

pub mod live;
pub mod redact;
pub mod server;
pub mod state;
pub mod transport;

pub use transport::{BIND_RETRY_INTERVAL, ControlPlaneHandle, start, start_retrying};

use serde::{Deserialize, Serialize};

use doggy_dns_basic::{
    NACOS_DEFAULT_DNS_ZONE, NACOS_DEFAULT_GROUP, NACOS_DEFAULT_NAMESPACE,
    NACOS_DEFAULT_SERVER_ADDR, PluginConfig, PluginKind,
};

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

/// Details object of a [`PluginSnapshot`], one struct per plugin kind.
///
/// The structs declare each kind's emitted keys and value types once; the
/// snapshot still stores the result as a `serde_json::Value` because the
/// `details` field is public and callers construct snapshots directly. That
/// round-trip also sorts the keys: `serde_json::Value`'s map is a `BTreeMap`
/// (no `preserve_order`), so `details` stays alphabetical, unlike the typed
/// envelopes that serialize in declaration order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NacosDetails {
    server_addr: String,
    namespace: String,
    group: String,
    dns_zone: String,
    ttl: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeDetails {
    cache_size: u32,
    min_ttl: u32,
    max_ttl: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ForwardDetails {
    upstream: Vec<String>,
    cache_size: u32,
    min_ttl: u32,
    max_ttl: u32,
}

/// Untagged so `details` stays a plain object with exactly the kind's keys.
#[derive(Serialize)]
#[serde(untagged)]
enum PluginDetails {
    Nacos(NacosDetails),
    Native(NativeDetails),
    Forward(ForwardDetails),
}

impl PluginSnapshot {
    pub fn from_config(order: usize, p: &PluginConfig) -> Self {
        let kind = p.kind.as_str().to_string();
        let details = match p.kind {
            PluginKind::Nacos => PluginDetails::Nacos(NacosDetails {
                server_addr: p
                    .server_addr
                    .as_deref()
                    .unwrap_or(NACOS_DEFAULT_SERVER_ADDR)
                    .to_string(),
                namespace: p
                    .namespace
                    .as_deref()
                    .unwrap_or(NACOS_DEFAULT_NAMESPACE)
                    .to_string(),
                group: p
                    .group
                    .as_deref()
                    .unwrap_or(NACOS_DEFAULT_GROUP)
                    .to_string(),
                dns_zone: p
                    .dns_zone
                    .as_deref()
                    .unwrap_or(NACOS_DEFAULT_DNS_ZONE)
                    .to_string(),
                ttl: p.ttl,
            }),
            PluginKind::Native => PluginDetails::Native(NativeDetails {
                cache_size: p.cache_size,
                min_ttl: p.min_ttl,
                max_ttl: p.max_ttl,
            }),
            PluginKind::Forward => PluginDetails::Forward(ForwardDetails {
                upstream: p.upstream.as_deref().unwrap_or(&[]).to_vec(),
                cache_size: p.cache_size,
                min_ttl: p.min_ttl,
                max_ttl: p.max_ttl,
            }),
        };
        Self {
            order,
            kind,
            enabled: p.enabled,
            details: serde_json::to_value(details).expect("plugin details are serializable"),
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
