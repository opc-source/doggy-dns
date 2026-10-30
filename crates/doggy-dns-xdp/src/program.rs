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
use std::path::{Path, PathBuf};

use crate::cache::DnsCacheMap;
use crate::interface::resolve_interface;
use crate::kernel_log::{TracingLog, XdpKernelLogger, kernel_log_level};
use crate::pin::{cache_shape_mismatch, declared_cache_shape, versioned_pin_path};
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

        // The pin path goes to the loader rather than to a `pin()` call after
        // load, because that is what makes a restart continue on the map that is
        // already pinned there: the loader opens the path first and only creates
        // and pins a map when that open fails. The path carries the cache layout
        // version for the same reason reuse does — entries survive the restart —
        // so a build whose entry layout changed starts cold instead of reading
        // what its predecessor wrote.
        let pin_path = versioned_pin_path(&config.pin_path);
        // Whether the path is already occupied is what tells the log below which
        // of the two stories is true.
        let reused_pinned_map = Path::new(&pin_path).exists();
        if let Some(parent) = Path::new(&pin_path).parent() {
            fs::create_dir_all(parent).ok();
        }

        let mut ebpf = EbpfLoader::new()
            // Apply the configured cache capacity at map-creation time; the
            // eBPF program only declares the default size. A reused map keeps
            // the capacity it was created with, which `log_cache_provenance`
            // reports.
            .map_max_entries("DNS_CACHE", config.cache_size)
            // The loader stores an owned `Cow<Path>`; `&str` and `String` are not
            // convertible, so this passes a `PathBuf` and leaves `pin_path`
            // available for the provenance log below.
            .map_pin_path("DNS_CACHE", PathBuf::from(&pin_path))
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

        // Take the DNS_CACHE map the loader created or reused: pinning already
        // happened, in whichever of those two ways.
        let map = ebpf
            .take_map("DNS_CACHE")
            .context("DNS_CACHE map not found in ELF")?;
        let cache = DnsCacheMap::from_map(map)?;
        log_cache_provenance(&cache, config.cache_size, &pin_path, reused_pinned_map);

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

/// Say which map the fast path ended up on, and what that means for the
/// configuration this process was given.
///
/// Reusing the pinned map is the ordinary restart, and the mismatch it can
/// report is routine rather than alarming: capacity in particular is fixed when
/// the map is first created, so editing `cache_size` on a node whose pin still
/// exists changes nothing until that file is removed. The loader warns instead
/// of refusing to start because the pinned map is working state — a node has to
/// come up after an upgrade — but a setting that did not take effect has to be
/// said out loud rather than left silent.
fn log_cache_provenance(
    cache: &DnsCacheMap,
    configured_entries: u32,
    pin_path: &str,
    reused_pinned_map: bool,
) {
    let shape = match cache.shape() {
        Ok(shape) => shape,
        Err(error) => {
            tracing::warn!(
                %error,
                %pin_path,
                "cannot read DNS_CACHE map info, so a reused map cannot be checked against this configuration"
            );
            return;
        }
    };

    if !reused_pinned_map {
        tracing::debug!(
            %pin_path,
            capacity = shape.max_entries,
            "created a cold DNS_CACHE map at the pin path"
        );
        return;
    }

    let mismatches = cache_shape_mismatch(&shape, &declared_cache_shape(configured_entries));
    if mismatches.is_empty() {
        tracing::info!(
            %pin_path,
            capacity = shape.max_entries,
            "reusing the pinned DNS_CACHE map; cached answers survive this restart"
        );
        return;
    }

    let details = mismatches
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    tracing::warn!(
        %pin_path,
        %details,
        configured_cache_size = configured_entries,
        "reusing the pinned DNS_CACHE map although it does not fit this configuration; \
         the pinned map's own values win until its file is removed"
    );
}
