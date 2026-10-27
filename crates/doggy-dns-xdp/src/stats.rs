//! XDP fast-path counters.
//!
//! The delta math compiles on every platform so it can be unit-tested without
//! root or eBPF. The map reader and poller are Linux-only.

#[cfg(target_os = "linux")]
use anyhow::{Context as _, Result};
#[cfg(target_os = "linux")]
use aya::maps::{Map, MapData, PerCpuArray};

use crate::types::XdpStats;

/// Difference between two [`XdpStats`] snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XdpStatsDelta {
    pub hit: u64,
    pub negative_hit: u64,
    pub miss: u64,
    pub expired: u64,
    pub drop: u64,
    /// Set when at least one counter went backwards, which only happens when
    /// the BPF map is recreated by a program reload.
    pub reset: bool,
}

impl XdpStatsDelta {
    /// Non-zero counters as `(dns.xdp.result label, count)` pairs.
    pub fn entries(&self) -> impl Iterator<Item = (&'static str, u64)> {
        [
            ("hit", self.hit),
            ("negative_hit", self.negative_hit),
            ("miss", self.miss),
            ("expired", self.expired),
            ("drop", self.drop),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
    }
}

impl XdpStats {
    /// Subtract a previous snapshot, handling counter resets per field.
    ///
    /// A field that went backwards is treated as a freshly recreated map: the
    /// current value becomes the delta instead of underflowing, and `reset` is
    /// set so the caller can log it.
    pub fn delta_from(&self, previous: &XdpStats) -> XdpStatsDelta {
        fn step(current: u64, previous: u64) -> (u64, bool) {
            if current >= previous {
                (current - previous, false)
            } else {
                (current, true)
            }
        }

        let (hit, hit_reset) = step(self.hit, previous.hit);
        let (negative_hit, negative_reset) = step(self.negative_hit, previous.negative_hit);
        let (miss, miss_reset) = step(self.miss, previous.miss);
        let (expired, expired_reset) = step(self.expired, previous.expired);
        let (drop, drop_reset) = step(self.drop, previous.drop);

        XdpStatsDelta {
            hit,
            negative_hit,
            miss,
            expired,
            drop,
            reset: hit_reset || negative_reset || miss_reset || expired_reset || drop_reset,
        }
    }
}

/// Userspace view of the per-CPU [`XdpStats`] map.
#[cfg(target_os = "linux")]
pub struct XdpStatsReader {
    inner: PerCpuArray<MapData, XdpStats>,
}

#[cfg(target_os = "linux")]
impl XdpStatsReader {
    /// Wrap the `XDP_STATS` map taken from the loaded ELF.
    pub fn from_map(map: Map) -> Result<Self> {
        Ok(Self {
            inner: PerCpuArray::try_from(map)?,
        })
    }

    /// Read every CPU slot and sum the counters.
    ///
    /// The slots are not read atomically with respect to each other, so a
    /// snapshot can lag a few microseconds behind the "current" total. Each
    /// slot only ever increases, so deltas stay non-negative.
    pub fn snapshot(&self) -> Result<XdpStats> {
        let values = self
            .inner
            .get(&0, 0)
            .context("failed to read XDP stats map")?;

        let mut total = XdpStats::default();
        for value in values.iter() {
            total.hit += value.hit;
            total.negative_hit += value.negative_hit;
            total.miss += value.miss;
            total.expired += value.expired;
            total.drop += value.drop;
        }
        Ok(total)
    }
}

/// Apply a snapshot to the running baseline and forward non-zero deltas.
pub fn emit_delta(
    previous: &mut XdpStats,
    current: XdpStats,
    sink: &mut impl FnMut(&'static str, u64),
) {
    let delta = current.delta_from(previous);
    if delta.reset {
        tracing::debug!("XDP stats counters reset (program reloaded)");
    }
    for (label, count) in delta.entries() {
        sink(label, count);
    }
    *previous = current;
}

#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::sync::watch;
#[cfg(target_os = "linux")]
use tokio::task::JoinHandle;

/// Spawn the periodic map reader.
///
/// `sink` receives `(dns.xdp.result label, delta)` for every non-zero field.
/// The task performs one final read when `stop` flips to `true`, then returns,
/// so the last partial window is not lost on shutdown.
#[cfg(target_os = "linux")]
pub fn spawn_stats_poller(
    reader: XdpStatsReader,
    period: Duration,
    mut stop: watch::Receiver<bool>,
    mut sink: impl FnMut(&'static str, u64) + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut previous = XdpStats::default();
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires immediately; consume it so the first emit lands
        // one full period after startup.
        ticker.tick().await;

        let mut warned = false;
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = stop.changed() => break,
            }

            match reader.snapshot() {
                Ok(current) => {
                    warned = false;
                    emit_delta(&mut previous, current, &mut sink);
                }
                Err(error) => {
                    // Keep the previous baseline so the next successful read
                    // still reports everything that happened in between.
                    if warned {
                        tracing::debug!(%error, "failed to read XDP stats map; skipping window");
                    } else {
                        tracing::warn!(%error, "failed to read XDP stats map; skipping window");
                        warned = true;
                    }
                }
            }
        }

        if let Ok(current) = reader.snapshot() {
            emit_delta(&mut previous, current, &mut sink);
        }
    })
}
