use anyhow::{Context as _, anyhow};

/// Detect the default network interface by parsing /proc/net/route.
///
/// Only works on Linux. On other platforms, returns an error because
/// `/proc/net/route` does not exist.
pub fn detect_default_interface() -> anyhow::Result<String> {
    let content =
        std::fs::read_to_string("/proc/net/route").context("failed to read /proc/net/route")?;
    parse_default_from_route(&content)
}

/// Parse the default interface name from /proc/net/route content.
///
/// The file format is tab-separated with a header line. The default route
/// has destination `00000000`. Returns the interface name from the first
/// matching line.
pub fn parse_default_from_route(content: &str) -> anyhow::Result<String> {
    for line in content.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && fields[1] == "00000000" {
            return Ok(fields[0].to_string());
        }
    }
    Err(anyhow!("no default route found in /proc/net/route"))
}

/// Resolve the XDP target interface with priority:
/// 1. `configured` parameter (from `xdp.interface` config)
/// 2. `XDP_INTERFACE` environment variable
/// 3. Auto-detect from `/proc/net/route`
pub fn resolve_interface(configured: &str) -> anyhow::Result<String> {
    if !configured.is_empty() {
        return Ok(configured.to_string());
    }
    if let Ok(iface) = std::env::var("XDP_INTERFACE")
        && !iface.is_empty()
    {
        return Ok(iface);
    }
    detect_default_interface()
}
