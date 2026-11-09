use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

type Probe = dyn Fn() -> Value + Send + Sync;

/// Standard signal shape for one Nacos zone.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NacosZoneSignal {
    subscribed_services: u64,
}

/// Fast-path signal shape for the XDP program.
#[derive(Serialize)]
struct XdpSignal {
    attached: bool,
}

/// Whole-snapshot envelope. The signals are keyed by registered name, so this
/// is the one object whose keys are not statically known; a `BTreeMap` keeps
/// the emitted order stable (sorted by name, as the hand-built map was — that
/// map sorted only because serde_json's default map is a `BTreeMap`, i.e. no
/// `preserve_order` feature in this workspace).
#[derive(Serialize)]
struct RuntimeSnapshot {
    signals: BTreeMap<String, Value>,
}

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

    /// Standard signal shape for one Nacos zone: `{"subscribedServices": n}`.
    ///
    /// The instance data lives in the Nacos client's cache, so this is the only
    /// thing doggy-dns knows about a zone: how many names it subscribed.
    pub fn register_nacos_zone(
        &mut self,
        zone: &str,
        subscribed_services: impl Fn() -> u64 + Send + Sync + 'static,
    ) {
        self.register(format!("nacos.{zone}"), move || {
            serde_json::to_value(NacosZoneSignal {
                subscribed_services: subscribed_services(),
            })
            .expect("zone signal is serializable")
        });
    }

    /// Fast-path signal: `{"attached": bool}` for the XDP program.
    pub fn register_xdp(&mut self, attached: impl Fn() -> bool + Send + Sync + 'static) {
        self.register("xdp", move || {
            serde_json::to_value(XdpSignal {
                attached: attached(),
            })
            .expect("xdp signal is serializable")
        });
    }

    /// Evaluate every probe once. Shape: `{"signals": {name: value, ...}}`.
    pub fn snapshot(&self) -> Value {
        let signals = self
            .probes
            .iter()
            .map(|(name, probe)| (name.clone(), probe()))
            .collect();
        serde_json::to_value(RuntimeSnapshot { signals }).expect("runtime snapshot is serializable")
    }
}
