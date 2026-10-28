use doggy_dns_basic::time::unix_now_secs;
use serde_json::{Map, Value};
use std::sync::Arc;

type Probe = dyn Fn() -> Value + Send + Sync;

/// Registry of live runtime signals behind the control plane's `/readyz`
/// endpoint and the MCP `get_runtime_status` tool.
///
/// Probes are cheap closures over shared atomics or lock-protected maps;
/// they run per request and must never block. Each registered probe returns
/// one JSON value under its name in the snapshot's `signals` object.
#[derive(Default)]
pub struct LiveState {
    probes: Vec<(String, Arc<Probe>)>,
}

impl LiveState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a named live signal. Names should be stable and namespaced
    /// (e.g. `nacos.<zone>`, `xdp`).
    pub fn register(
        &mut self,
        name: impl Into<String>,
        probe: impl Fn() -> Value + Send + Sync + 'static,
    ) {
        self.probes.push((name.into(), Arc::new(probe)));
    }

    /// Standard signal shape for one Nacos zone:
    /// `{"cachedServices": n, "lastPushSecondsAgo": null | seconds}`.
    /// `last_push_unix` returning 0 means "no push has ever arrived" and is
    /// reported as `null` rather than a misleading number.
    pub fn register_nacos_zone(
        &mut self,
        zone: &str,
        cached_services: impl Fn() -> u64 + Send + Sync + 'static,
        last_push_unix: impl Fn() -> i64 + Send + Sync + 'static,
    ) {
        self.register(format!("nacos.{zone}"), move || {
            let ts = last_push_unix();
            let freshness = if ts == 0 {
                Value::Null
            } else {
                Value::from(unix_now_secs().saturating_sub(ts))
            };
            // Built by hand: `json!` expands through `Result::unwrap`, which
            // .clippy.toml disallows.
            let mut signal = Map::new();
            signal.insert("cachedServices".to_string(), Value::from(cached_services()));
            signal.insert("lastPushSecondsAgo".to_string(), freshness);
            Value::Object(signal)
        });
    }

    /// Fast-path signal: `{"attached": bool}` for the XDP program.
    pub fn register_xdp(&mut self, attached: impl Fn() -> bool + Send + Sync + 'static) {
        self.register("xdp", move || {
            let mut signal = Map::new();
            signal.insert("attached".to_string(), Value::from(attached()));
            Value::Object(signal)
        });
    }

    /// Evaluate every probe once. Shape: `{"signals": {name: value, ...}}`.
    pub fn snapshot(&self) -> Value {
        let mut signals = Map::new();
        for (name, probe) in &self.probes {
            signals.insert(name.clone(), probe());
        }
        let mut root = Map::new();
        root.insert("signals".to_string(), Value::Object(signals));
        Value::Object(root)
    }
}
