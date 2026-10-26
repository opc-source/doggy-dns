//! XDP program loader and lifecycle manager.
//!
//! This module is only available on Linux because it depends on the `aya`
//! crate for loading and attaching eBPF programs.

#[cfg(target_os = "linux")]
use anyhow::{Context as _, Result};
#[cfg(target_os = "linux")]
use aya::EbpfLoader;
#[cfg(target_os = "linux")]
use aya::programs::{Xdp, XdpFlags};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use crate::cache::DnsCacheMap;
#[cfg(target_os = "linux")]
use crate::interface::resolve_interface;
#[cfg(target_os = "linux")]
use doggy_dns_core::config::XdpConfig;

/// Manages the XDP eBPF program lifecycle: loading, map setup, and attach.
///
/// The [`XdpManager`] holds ownership of the loaded eBPF object and the
/// optional link ID (if attached). It also exposes the [`DnsCacheMap`] for
/// the middleware to write cache entries.
#[cfg(target_os = "linux")]
pub struct XdpManager {
    _ebpf: aya::Ebpf,
    _link: Option<aya::programs::xdp::XdpLinkId>,
    pub cache: DnsCacheMap,
    pub interface: String,
}

#[cfg(target_os = "linux")]
impl XdpManager {
    /// Load the XDP program from the embedded ELF, set up the BPF map, and
    /// optionally attach to the configured network interface.
    pub fn new(config: &XdpConfig) -> Result<Self> {
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
        let map_data = ebpf
            .take_map("DNS_CACHE")
            .context("DNS_CACHE map not found in ELF")?;
        map_data
            .pin(pin_path)
            .context("failed to pin DNS_CACHE map")?;

        let cache = DnsCacheMap::from_map_data(map_data)?;

        // Load and optionally attach the XDP program
        let link_id = if config.attach_mode == "auto" {
            let program: &mut Xdp = ebpf
                .program_mut("dns_xdp")
                .context("dns_xdp program not found")?
                .try_into()
                .context("dns_xdp is not an XDP program")?;

            program.load().context("failed to load XDP program")?;
            let link_id = program
                .attach(&iface, XdpFlags::default())
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
            cache,
            interface: iface,
        })
    }
}
