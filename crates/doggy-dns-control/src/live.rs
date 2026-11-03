use doggy_dns_basic::time::unix_now_secs;
use serde_json::{Map, Value};
use std::sync::{Arc, RwLock};

type Probe = dyn Fn() -> Value + Send + Sync;

/// One named live signal, as returned by [`nacos_zone_signal`].
pub type LiveProbe = (String, Arc<Probe>);

/// Registry of live runtime signals behind the control plane's `/readyz`
/// endpoint and the MCP `get_runtime_status` tool.
///
/// Probes are cheap closures over shared atomics or lock-protected maps;
/// they run per request and must never block. Each registered probe returns
/// one JSON value under its name in the snapshot's `signals` object.
///
/// The set is interior-mutable because a config reload changes which Nacos
/// zones exist: [`Self::replace_nacos_zones`] swaps them without rebuilding
/// the control plane.
#[derive(Default)]
pub struct LiveState {
    probes: RwLock<Vec<(String, Arc<Probe>)>>,
}

/// The standard signal for one Nacos zone, named `nacos.<zone>`:
/// `{"cachedServices": n, "lastPushSecondsAgo": null | seconds}`.
/// `last_push_unix` returning 0 means "no push has ever arrived" and is
/// reported as `null` rather than a misleading number.
pub fn nacos_zone_signal(
    zone: &str,
    cached_services: impl Fn() -> u64 + Send + Sync + 'static,
    last_push_unix: impl Fn() -> i64 + Send + Sync + 'static,
) -> LiveProbe {
    let probe = move || {
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
    };
    (format!("{NACOS_PREFIX}{zone}"), Arc::new(probe))
}

const NACOS_PREFIX: &str = "nacos.";

impl LiveState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a named live signal. Names should be stable and namespaced
    /// (e.g. `nacos.<zone>`, `xdp`).
    pub fn register(
        &self,
        name: impl Into<String>,
        probe: impl Fn() -> Value + Send + Sync + 'static,
    ) {
        self.write().push((name.into(), Arc::new(probe)));
    }

    /// Standard signal for one Nacos zone, named `nacos.<zone>`.
    pub fn register_nacos_zone(
        &self,
        zone: &str,
        cached_services: impl Fn() -> u64 + Send + Sync + 'static,
        last_push_unix: impl Fn() -> i64 + Send + Sync + 'static,
    ) {
        self.push(nacos_zone_signal(zone, cached_services, last_push_unix));
    }

    /// Append an already-built signal.
    pub fn push(&self, probe: LiveProbe) {
        self.write().push(probe);
    }

    /// Replace every `nacos.<zone>` signal with the zones now in force.
    ///
    /// Called after a successful config reload, whose new chain may have
    /// added, dropped, or rebuilt zones; the old watchers' caches are gone, so
    /// leaving their probes registered would report numbers nothing owns.
    pub fn replace_nacos_zones(&self, zones: Vec<LiveProbe>) {
        let mut probes = self.write();
        probes.retain(|(name, _)| !name.starts_with(NACOS_PREFIX));
        probes.extend(zones);
    }

    /// Fast-path signal: `{"attached": bool}` for the XDP program.
    pub fn register_xdp(&self, attached: impl Fn() -> bool + Send + Sync + 'static) {
        self.register("xdp", move || {
            let mut signal = Map::new();
            signal.insert("attached".to_string(), Value::from(attached()));
            Value::Object(signal)
        });
    }

    /// Evaluate every probe once. Shape: `{"signals": {name: value, ...}}`.
    pub fn snapshot(&self) -> Value {
        let mut signals = Map::new();
        for (name, probe) in self.read().iter() {
            signals.insert(name.clone(), probe());
        }
        let mut root = Map::new();
        root.insert("signals".to_string(), Value::Object(signals));
        Value::Object(root)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<(String, Arc<Probe>)>> {
        self.probes
            .read()
            .expect("live probe registry is never poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<(String, Arc<Probe>)>> {
        self.probes
            .write()
            .expect("live probe registry is never poisoned")
    }
}
