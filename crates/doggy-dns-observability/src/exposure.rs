//! Node identity published on every OTel signal: host name, the address the
//! node is reachable on, and the listener ports it exposes.

use std::collections::BTreeMap;
use std::net::UdpSocket;

use doggy_dns_basic::{ControlConfig, ServerConfig};

/// Hostname from the environment, falling back to the OS hostname and finally
/// "unknown". Shells rarely export `HOSTNAME`, so the OS lookup is what keeps
/// `host.name` meaningful.
pub fn detect_hostname() -> String {
    detect_hostname_with(|name| std::env::var(name).ok())
}

pub(crate) fn detect_hostname_with(env: impl Fn(&str) -> Option<String>) -> String {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|name| {
            env(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(os_hostname)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Attributes describing where this node is reachable. Explicit
/// `resource_attributes` entries override any of them.
pub(crate) fn node_attributes(
    server: &ServerConfig,
    control: &ControlConfig,
) -> BTreeMap<String, String> {
    let listen_addr = if control.enabled {
        &control.listen_addr
    } else {
        &server.listen_addr
    };
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "doggy_dns.ip_addr".to_string(),
        advertised_ip_addr(listen_addr),
    );
    attributes.insert("doggy_dns.dns.port".to_string(), server.port.to_string());
    if let Some(tls) = server.tls.as_ref().filter(|tls| tls.enabled) {
        attributes.insert("doggy_dns.tls.port".to_string(), tls.port.to_string());
    }
    if let Some(https) = server.https.as_ref().filter(|https| https.enabled) {
        attributes.insert("doggy_dns.https.port".to_string(), https.port.to_string());
    }
    if control.enabled {
        attributes.insert(
            "doggy_dns.control.port".to_string(),
            control.port.to_string(),
        );
    }
    attributes
}

/// Address the console can dial. A concrete bind address is used as-is, a
/// loopback bind stays loopback (only the same machine can dial it), and a
/// wildcard bind resolves the egress interface address, then loopback.
fn advertised_ip_addr(listen_addr: &str) -> String {
    advertised_ip_addr_with(listen_addr, detect_ip_addr)
}

fn advertised_ip_addr_with(listen_addr: &str, detect: impl FnOnce() -> Option<String>) -> String {
    match listen_addr.trim() {
        "127.0.0.1" | "::1" | "[::1]" | "localhost" => "127.0.0.1".to_string(),
        "0.0.0.0" | "::" | "[::]" => detect().unwrap_or_else(|| "127.0.0.1".to_string()),
        other => other.trim_matches(['[', ']']).to_string(),
    }
}

/// Egress interface address; connecting a UDP socket only selects a route and
/// sends no packets.
fn detect_ip_addr() -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    (!addr.ip().is_loopback() && !addr.ip().is_unspecified()).then(|| addr.ip().to_string())
}

#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes and the call only writes
    // up to that length; a non-zero return means the buffer is untouched.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..end])
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn server(listen_addr: &str, port: u16) -> ServerConfig {
        ServerConfig {
            listen_addr: listen_addr.to_string(),
            port,
            tcp_timeout: 10,
            shutdown_timeout: 5,
            worker_threads: 1,
            rotate_answers: true,
            tls: None,
            https: None,
        }
    }

    #[test]
    fn hostname_prefers_the_environment() {
        assert_eq!(
            detect_hostname_with(env_map(&[("HOSTNAME", "node-a")])),
            "node-a"
        );
    }

    #[test]
    fn hostname_falls_back_to_computer_name() {
        assert_eq!(
            detect_hostname_with(env_map(&[("COMPUTERNAME", "win-node")])),
            "win-node"
        );
    }

    #[test]
    fn blank_environment_values_are_skipped() {
        assert_eq!(
            detect_hostname_with(env_map(&[("HOSTNAME", "  "), ("COMPUTERNAME", "win-node")])),
            "win-node"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hostname_falls_back_to_the_os() {
        let detected = detect_hostname_with(env_map(&[]));
        let expected = os_hostname().unwrap_or_else(|| "unknown".to_string());
        assert_eq!(detected, expected);
        assert_ne!(detected, "");
    }

    #[test]
    fn concrete_bind_address_is_advertised_as_is() {
        assert_eq!(
            advertised_ip_addr_with("10.0.0.5", || Some("10.1.1.1".to_string())),
            "10.0.0.5"
        );
    }

    #[test]
    fn loopback_bind_address_stays_loopback() {
        assert_eq!(
            advertised_ip_addr_with("127.0.0.1", || Some("10.1.1.1".to_string())),
            "127.0.0.1"
        );
        assert_eq!(
            advertised_ip_addr_with("::1", || Some("10.1.1.1".to_string())),
            "127.0.0.1"
        );
    }

    #[test]
    fn wildcard_bind_address_resolves_the_egress_ip() {
        assert_eq!(
            advertised_ip_addr_with("0.0.0.0", || Some("10.1.1.1".to_string())),
            "10.1.1.1"
        );
    }

    #[test]
    fn wildcard_bind_address_falls_back_to_loopback() {
        assert_eq!(advertised_ip_addr_with("0.0.0.0", || None), "127.0.0.1");
    }

    #[test]
    fn ipv6_bind_address_drops_the_brackets() {
        assert_eq!(
            advertised_ip_addr_with("[2001:db8::1]", || None),
            "2001:db8::1"
        );
    }

    #[test]
    fn node_attributes_publish_the_dns_listener_and_optional_ports() {
        let mut server = server("127.0.0.1", 5353);
        assert!(server.tls.is_none());
        let control = ControlConfig::default();

        let attributes = node_attributes(&server, &control);
        assert_eq!(
            attributes.get("doggy_dns.ip_addr").map(String::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(
            attributes.get("doggy_dns.dns.port").map(String::as_str),
            Some("5353")
        );
        assert!(!attributes.contains_key("doggy_dns.control.port"));
        assert!(!attributes.contains_key("doggy_dns.tls.port"));
        assert!(!attributes.contains_key("doggy_dns.https.port"));

        server.tls = Some(doggy_dns_basic::TlsConfig {
            enabled: true,
            port: 8853,
            cert_path: String::new(),
            key_path: String::new(),
        });
        server.https = Some(doggy_dns_basic::HttpsConfig {
            enabled: true,
            port: 8443,
            cert_path: String::new(),
            key_path: String::new(),
        });
        let control = ControlConfig {
            enabled: true,
            listen_addr: "127.0.0.1".to_string(),
            port: 19090,
            ..ControlConfig::default()
        };
        let attributes = node_attributes(&server, &control);
        assert_eq!(
            attributes.get("doggy_dns.control.port").map(String::as_str),
            Some("19090")
        );
        assert_eq!(
            attributes.get("doggy_dns.tls.port").map(String::as_str),
            Some("8853")
        );
        assert_eq!(
            attributes.get("doggy_dns.https.port").map(String::as_str),
            Some("8443")
        );

        // The port identifies the control listener itself, so it is published
        // even when `mcp` is not among the served endpoints.
        let control = ControlConfig {
            endpoints: vec!["healthz".to_string()],
            ..control
        };
        let attributes = node_attributes(&server, &control);
        assert_eq!(
            attributes.get("doggy_dns.control.port").map(String::as_str),
            Some("19090")
        );
    }
}
