//! Userspace reader and periodic poller for the per-CPU XDP stats map
//! (Linux only).
//!
//! This submodule is gated by `stats.rs`; everything here may use `aya`
//! without per-item `cfg` attributes.

use super::emit_delta;
use crate::types::XdpStats;
use anyhow::{Context as _, Result};
use aya::maps::{Map, MapData, PerCpuArray};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Userspace view of the per-CPU [`XdpStats`] map.
pub struct XdpStatsReader {
    inner: PerCpuArray<MapData, XdpStats>,
}

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

/// Spawn the periodic map reader.
///
/// `sink` receives `(dns.xdp.result label, delta)` for every non-zero field.
/// The task performs one final read when `stop` flips to `true`, then returns,
/// so the last partial window is not lost on shutdown.
///
/// `period` must be non-zero; `tokio::time::interval` panics on
/// `Duration::ZERO`. The binary clamps it with `interval_secs.max(1)`.
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
