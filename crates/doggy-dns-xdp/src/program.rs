//! XDP program loader and lifecycle manager.
//!
//! Linux-only: the module is gated once at `lib.rs` because it depends on the
//! `aya` crate for loading and attaching eBPF programs, so no item inside
//! needs its own `cfg` attribute.

use anyhow::{Context as _, Result};
use aya::EbpfLoader;
use aya::maps::{Array, MapData};
use aya::programs::{Xdp, XdpMode};
use aya_log::EbpfLogger;
use std::fs;
use std::path::Path;

use crate::cache::DnsCacheMap;
use crate::interface::resolve_interface;
use crate::kernel_log::{TracingLog, XdpKernelLogger, kernel_log_level};
use crate::stats::XdpStatsReader;
use crate::types::XdpRuntimeConfig;
use doggy_dns_basic::XdpConfig;

/// Manages the XDP eBPF program lifecycle: loading, map setup, and attach.
///
/// The [`XdpManager`] holds ownership of the loaded eBPF object and the
/// optional link ID (if attached). It also exposes the [`DnsCacheMap`] for
/// the middleware to write cache entries.
pub struct XdpManager {
    _ebpf: aya::Ebpf,
    _link: Option<aya::programs::xdp::XdpLinkId>,
    cache: Option<DnsCacheMap>,
    stats: Option<XdpStatsReader>,
    logger: Option<XdpKernelLogger>,
    pub interface: String,
}

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

        let level = kernel_log_level();
        let mut ebpf = EbpfLoader::new()
            // Apply the configured cache capacity at map-creation time; the
            // eBPF program only declares the default size.
            .map_max_entries("DNS_CACHE", config.cache_size)
            // Bakes the verbosity into the program before the verifier runs.
            // `must_exist = false`: a build without log statements has no
            // symbol, and the override is then a no-op.
            .override_global(aya_log::LEVEL, &level, false)
            .load(elf_bytes)
            .context("failed to load eBPF ELF")?;

        // Take the AYA_LOGS ring buffer before the program loads: dropping the
        // logger closes the map FD and makes loading the program fail. A
        // missing map only means this build carries no log statements, which
        // must not disable the fast path.
        let logger = match EbpfLogger::init_with_logger(&mut ebpf, TracingLog) {
            Ok(logger) => Some(logger),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "XDP kernel-side logging unavailable, continuing without kernel logs"
                );
                None
            }
        };
        tracing::debug!(level, "XDP kernel-side log level set");

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

        // Load the program; attach only in auto mode (manual mode leaves it
        // loaded but unattached for an external binder).
        let program = load_dns_program(&mut ebpf)?;
        program.load().context("failed to load XDP program")?;
        let link_id = if config.attach_mode == "auto" {
            Some(
                program
                    .attach(&iface, XdpMode::Default)
                    .context(format!("failed to attach XDP to interface {iface}"))?,
            )
        } else {
            None
        };

        Ok(Self {
            _ebpf: ebpf,
            _link: link_id,
            cache: Some(cache),
            stats: Some(stats),
            logger,
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

    /// Take the kernel log consumer, if the loaded program exposes the
    /// `AYA_LOGS` map. Returns `None` at most once, and immediately when the
    /// build carries no log statements.
    pub fn take_log(&mut self) -> Option<XdpKernelLogger> {
        self.logger.take()
    }
}

/// Fetch the `dns_xdp` program from a loaded ELF object, verifying it is an
/// XDP program. The program is returned unloaded; the caller decides whether
/// to attach it after [`Xdp::load`].
fn load_dns_program(ebpf: &mut aya::Ebpf) -> Result<&mut Xdp> {
    ebpf.program_mut("dns_xdp")
        .context("dns_xdp program not found")?
        .try_into()
        .context("dns_xdp is not an XDP program")
}
