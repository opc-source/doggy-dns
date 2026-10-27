//! XDP program loader and lifecycle manager.
//!
//! This module is only available on Linux because it depends on the `aya`
//! crate for loading and attaching eBPF programs.

#[cfg(target_os = "linux")]
use anyhow::{Context as _, Result};
#[cfg(target_os = "linux")]
use aya::EbpfLoader;
#[cfg(target_os = "linux")]
use aya::maps::{Array, MapData};
#[cfg(target_os = "linux")]
use aya::programs::{Xdp, XdpMode};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use crate::cache::DnsCacheMap;
#[cfg(target_os = "linux")]
use crate::interface::resolve_interface;
#[cfg(target_os = "linux")]
use crate::stats::XdpStatsReader;
#[cfg(target_os = "linux")]
use crate::types::XdpRuntimeConfig;
#[cfg(target_os = "linux")]
use doggy_dns_basic::XdpConfig;

/// Manages the XDP eBPF program lifecycle: loading, map setup, and attach.
///
/// The [`XdpManager`] holds ownership of the loaded eBPF object and the
/// optional link ID (if attached). It also exposes the [`DnsCacheMap`] for
/// the middleware to write cache entries.
#[cfg(target_os = "linux")]
pub struct XdpManager {
    _ebpf: aya::Ebpf,
    _link: Option<aya::programs::xdp::XdpLinkId>,
    cache: Option<DnsCacheMap>,
    stats: Option<XdpStatsReader>,
    pub interface: String,
}

#[cfg(target_os = "linux")]
impl XdpManager {
    /// Load the XDP program from the embedded ELF, set up the BPF maps, and
    /// optionally attach to the configured network interface.
    ///
    /// `dns_port` must be the UDP port the userspace server listens on: the
    /// program only answers queries addressed to it.
    pub fn new(config: &XdpConfig, dns_port: u16) -> Result<Self> {
        let iface =
            resolve_interface(&config.interface).context("failed to resolve XDP interface")?;

        let elf_bytes = include_bytes!(env!("XDP_EBPF_ELF"));
        if elf_bytes.is_empty() {
            anyhow::bail!("eBPF ELF is empty — build the eBPF program first");
        }

        let mut ebpf = EbpfLoader::new()
            .load(elf_bytes)
            .context("failed to load eBPF ELF")?;

        // Ensure the pin directory exists
        let pin_path = Path::new(&config.pin_path);
        if let Some(parent) = pin_path.parent() {
            fs::create_dir_all(parent).ok();
        }

        // Take the DNS_CACHE map from the ELF and pin it
        let map = ebpf
            .take_map("DNS_CACHE")
            .context("DNS_CACHE map not found in ELF")?;
        map.pin(pin_path).context("failed to pin DNS_CACHE map")?;

        let cache = DnsCacheMap::from_map(map)?;

        // Take the stats map and wrap it before attach so the reader is ready
        // as soon as the program starts counting.
        let stats = ebpf
            .take_map("XDP_STATS")
            .context("XDP_STATS map not found in ELF")?;
        let stats = XdpStatsReader::from_map(stats).context("failed to read XDP_STATS map")?;

        // Fill in the runtime configuration before attach so the program never
        // observes a half-initialised entry.
        let mut config_map: Array<MapData, XdpRuntimeConfig> = Array::try_from(
            ebpf.take_map("XDP_CONFIG")
                .context("XDP_CONFIG map not found in ELF")?,
        )
        .context("XDP_CONFIG has an unexpected value type")?;
        config_map
            .set(0, XdpRuntimeConfig { dns_port }, 0)
            .context("failed to write XDP runtime configuration")?;

        // Load and optionally attach the XDP program
        let link_id = if config.attach_mode == "auto" {
            let program: &mut Xdp = ebpf
                .program_mut("dns_xdp")
                .context("dns_xdp program not found")?
                .try_into()
                .context("dns_xdp is not an XDP program")?;

            program.load().context("failed to load XDP program")?;
            let link_id = program
                .attach(&iface, XdpMode::Default)
                .context(format!("failed to attach XDP to interface {iface}"))?;
            Some(link_id)
        } else {
            // manual mode: program is loaded but not attached
            let program: &mut Xdp = ebpf
                .program_mut("dns_xdp")
                .context("dns_xdp program not found")?
                .try_into()
                .context("dns_xdp is not an XDP program")?;
            program.load().context("failed to load XDP program")?;
            None
        };

        Ok(Self {
            _ebpf: ebpf,
            _link: link_id,
            cache: Some(cache),
            stats: Some(stats),
            interface: iface,
        })
    }

    /// Take ownership of the [`DnsCacheMap`] so it can be moved into the
    /// middleware. Panics (via `expect`) if called more than once.
    pub fn take_cache(&mut self) -> DnsCacheMap {
        self.cache
            .take()
            .expect("XDP cache already taken or was never initialized")
    }

    /// Take ownership of the [`XdpStatsReader`]. Panics (via `expect`) if called
    /// more than once, matching [`Self::take_cache`].
    pub fn take_stats(&mut self) -> XdpStatsReader {
        self.stats
            .take()
            .expect("XDP stats reader already taken or was never initialized")
    }
}
