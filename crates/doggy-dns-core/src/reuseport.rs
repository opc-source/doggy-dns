//! SO_REUSEPORT UDP socket binding.
//!
//! Uses raw `libc` calls instead of `socket2` — the operations we need
//! (`socket`, `setsockopt`, `fcntl`, `bind`) are thin wrappers, and we
//! already depend on `libc` for `SO_REUSEPORT`.
//!
//! The entire module is conditionally compiled: on Linux it provides the
//! real implementation; on other platforms it exposes a stub that always
//! returns an error (config validation should prevent this path anyway).

use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::Context;
use tokio::net::UdpSocket;

#[cfg(target_os = "linux")]
pub fn bind_reuseport_udp(addr: std::net::SocketAddr) -> Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    let fd = create_raw_socket(addr, libc::SOCK_DGRAM, libc::IPPROTO_UDP)?;
    if let Err(e) = set_reuse_addr(fd) {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    if let Err(e) = set_reuse_port(fd) {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    if let Err(e) = set_nonblocking(fd) {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    if let Err(e) = bind_raw(fd, addr) {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
    UdpSocket::from_std(std_socket).map_err(|e| anyhow::anyhow!(e))
}

#[cfg(not(target_os = "linux"))]
pub fn bind_reuseport_udp(_addr: std::net::SocketAddr) -> Result<UdpSocket> {
    anyhow::bail!("SO_REUSEPORT UDP binding is only supported on Linux")
}

// ---------------------------------------------------------------------------
// Linux-only helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn create_raw_socket(
    addr: std::net::SocketAddr,
    sock_type: libc::c_int,
    protocol: libc::c_int,
) -> Result<libc::c_int> {
    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(domain, sock_type, protocol) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("libc::socket() failed");
    }
    Ok(fd)
}

#[cfg(target_os = "linux")]
fn set_reuse_addr(fd: libc::c_int) -> Result<()> {
    set_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR)
}

#[cfg(target_os = "linux")]
fn set_reuse_port(fd: libc::c_int) -> Result<()> {
    set_sockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT)
}

#[cfg(target_os = "linux")]
fn set_sockopt_int(fd: libc::c_int, level: libc::c_int, name: libc::c_int) -> Result<()> {
    let val: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &val as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(std::io::Error::last_os_error())
            .context(format!("setsockopt(level={level}, name={name}) failed"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_nonblocking(fd: libc::c_int) -> Result<()> {
    let mut flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL) failed");
    }
    flags |= libc::O_NONBLOCK;
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFL, O_NONBLOCK) failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind_raw(fd: libc::c_int, addr: std::net::SocketAddr) -> Result<()> {
    match addr {
        std::net::SocketAddr::V4(v4) => {
            let sa = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(v4.ip().octets()),
                },
                sin_zero: [0; 8],
            };
            let ret = unsafe {
                libc::bind(
                    fd,
                    &sa as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            if ret < 0 {
                return Err(std::io::Error::last_os_error()).context("libc::bind() failed");
            }
        }
        std::net::SocketAddr::V6(v6) => {
            let sa = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: 0,
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: 0,
            };
            let ret = unsafe {
                libc::bind(
                    fd,
                    &sa as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            };
            if ret < 0 {
                return Err(std::io::Error::last_os_error()).context("libc::bind() failed");
            }
        }
    }
    Ok(())
}
