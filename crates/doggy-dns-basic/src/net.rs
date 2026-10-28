//! Socket address parsing shared by the DNS, TLS, HTTPS and MCP listeners.
//!
//! Building an address with `format!("{host}:{port}")` breaks for IPv6:
//! `::` becomes `:::15353`, which is not a valid socket address.

use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr};

/// Pair an IPv4/IPv6 literal with `port`.
///
/// Bracketed IPv6 (`[::1]`) is accepted for symmetry with URLs. Hostnames are
/// rejected: the config expects a literal bind address.
pub fn socket_addr(host: &str, port: u16) -> Result<SocketAddr> {
    let host = host.trim();
    let literal = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    let ip: IpAddr = literal
        .parse()
        .with_context(|| format!("'{host}' is not an IPv4 or IPv6 address"))?;
    Ok(SocketAddr::new(ip, port))
}
