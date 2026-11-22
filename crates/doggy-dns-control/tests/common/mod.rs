#![allow(dead_code)]
#![allow(clippy::disallowed_methods)]

//! Shared fixtures for the `doggy-dns-control` test targets.
//!
//! The three local copies this replaces differed only in the plugin list and the
//! raw config text, so those are arguments; the instance identity, the endpoint
//! list and the registered live signal are what every target reports.

use chrono::Utc;
use doggy_dns_control::live::LiveState;
use doggy_dns_control::state::McpState;
use doggy_dns_control::{LogLevelAdmin, LogLevelState, PluginSnapshot};
use std::sync::{Arc, Mutex};

/// The running instance every control test reports.
pub fn test_state(plugins: Vec<PluginSnapshot>, config_text: &str) -> McpState {
    test_state_with_logs(plugins, config_text, FakeLogs::new("info"))
}

/// Like [`test_state`], with the log control the fixture reports.
pub fn test_state_with_logs(
    plugins: Vec<PluginSnapshot>,
    config_text: &str,
    log_control: Arc<dyn LogLevelAdmin>,
) -> McpState {
    McpState::from_parts(
        "dns-node-1".into(),
        4242,
        "0.1.0".into(),
        Utc::now(),
        "0.0.0.0:15353".into(),
        vec!["udp".into(), "tcp".into()],
        plugins,
        config_text,
        log_control,
    )
}

/// A log control with one recorded output, so the tools' wiring can be tested
/// without installing a subscriber.
pub struct FakeLogs {
    baseline: String,
    directives: Mutex<String>,
}

impl FakeLogs {
    pub fn new(directives: &str) -> Arc<Self> {
        Arc::new(Self {
            baseline: directives.to_string(),
            directives: Mutex::new(directives.to_string()),
        })
    }

    fn state(&self) -> LogLevelState {
        LogLevelState {
            stdout: Some(self.directives.lock().unwrap().clone()),
            otlp: None,
        }
    }
}

impl LogLevelAdmin for FakeLogs {
    fn current(&self) -> LogLevelState {
        self.state()
    }

    fn set(&self, directives: &str) -> Result<LogLevelState, String> {
        if directives == "bogus" {
            return Err("invalid filter directive".to_string());
        }
        *self.directives.lock().unwrap() = directives.to_string();
        Ok(self.state())
    }

    fn reset(&self) -> Result<LogLevelState, String> {
        *self.directives.lock().unwrap() = self.baseline.clone();
        Ok(self.state())
    }
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
