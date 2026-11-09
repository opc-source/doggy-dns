#![allow(dead_code)]
#![allow(clippy::disallowed_methods)]

//! Shared fixtures for the `doggy-dns-control` test targets.
//!
//! The three local copies this replaces differed only in the plugin list and the
//! raw config text, so those are arguments; the instance identity, the endpoint
//! list and the registered live signal are what every target reports.

use chrono::Utc;
use doggy_dns_control::PluginSnapshot;
use doggy_dns_control::live::LiveState;
use doggy_dns_control::state::McpState;
use std::sync::Arc;

/// The running instance every control test reports.
pub fn test_state(plugins: Vec<PluginSnapshot>, config_text: &str) -> McpState {
    McpState::from_parts(
        "dns-node-1".into(),
        4242,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        plugins,
        config_text,
    )
}

/// The one forward plugin the fixtures list.
pub fn forward_plugins() -> Vec<PluginSnapshot> {
    vec![PluginSnapshot {
        order: 0,
        kind: "forward".into(),
        enabled: true,
        details: serde_json::json!({"upstream": ["8.8.8.8:53"]}),
    }]
}

/// One registered signal, so the ready-time report has something to show.
pub fn test_live() -> Arc<LiveState> {
    let mut live = LiveState::new();
    live.register(
        "nacos.nacos.local",
        || serde_json::json!({"subscribedServices": 3}),
    );
    Arc::new(live)
}
