//! Socket binding with `SO_REUSEPORT`.
//!
//! A restart can overlap two processes on one address only if *every* socket
//! bound to that address asked for `SO_REUSEPORT` before its `bind()`: the
//! option is fixed at bind time and cannot be added by whoever arrives later.
//! It is therefore set unconditionally, and `reuseport_listen_num` decides only
//! how many UDP receive paths this process opens — the fan-out is UDP-only,
//! because that is where the query volume is; TCP keeps one listener per port.
//!
//! Non-Linux platforms do not request the option and keep the plain bind, so
//! they still build and run — they just have no overlap window. A second socket
//! on one address is impossible there, which is why `reuseport_listen_num > 1`
//! remains a Linux-only configuration.
//!
//! Uses raw `libc` calls instead of `socket2` — the operations we need
//! (`socket`, `setsockopt`, `fcntl`, `bind`, `listen`) are thin wrappers, and we
//! already depend on `libc` for `SO_REUSEPORT`.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use tokio::net::{TcpListener, UdpSocket};

/// Accept backlog matching `tokio::net::TcpListener::bind`, which delegates to
/// mio and asks for 128 on Linux (mio 1.2.4, `net/tcp/listener.rs`). Taking the
/// fd over into this module moves the `listen()` call here, so the value has to
/// be repeated deliberately rather than inherited silently.
#[cfg(target_os = "linux")]
const LISTEN_BACKLOG: libc::c_int = 128;

/// Whether this platform can overlap a restart with its predecessor. A property
/// of the platform rather than of the configuration, so the startup log states
/// it: `reuseport` means the option was set, not merely that several sockets
/// were opened. It is public because the control plane reads it too: that
/// listener deliberately stays out of the group, so whether the DNS port can
/// overlap at all decides whether its bind is a race or a certainty.
pub const DNS_PORT_OVERLAP_SUPPORTED: bool = cfg!(target_os = "linux");

/// Bind a UDP socket for the DNS server. `reuseport_listen_num` decides how
/// many of these one process opens; only UDP has the volume to want several.
#[cfg(target_os = "linux")]
pub async fn bind_udp(addr: SocketAddr) -> Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    let fd = bind_raw_socket(addr, libc::SOCK_DGRAM, libc::IPPROTO_UDP)
        .with_context(|| format!("failed to bind UDP socket with SO_REUSEPORT on {addr}"))?;
    // `from_raw_fd` takes ownership here, so any failure below closes the fd
    // along with the std socket instead of leaking it.
    let std_socket = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
    UdpSocket::from_std(std_socket).context("failed to register the UDP socket with the runtime")
}

/// Bind a UDP socket for the DNS server.
#[cfg(not(target_os = "linux"))]
pub async fn bind_udp(addr: SocketAddr) -> Result<UdpSocket> {
    UdpSocket::bind(addr)
        .await
        .with_context(|| format!("failed to bind UDP socket on {addr}"))
}

/// Bind a single TCP listener, for the DNS, TLS or HTTPS server: the fan-out
/// above is UDP only, so TCP gets one address holder per process.
#[cfg(target_os = "linux")]
pub async fn bind_tcp(addr: SocketAddr) -> Result<TcpListener> {
    use std::os::fd::FromRawFd;

    let fd = bind_raw_socket(addr, libc::SOCK_STREAM, libc::IPPROTO_TCP)
        .with_context(|| format!("failed to bind TCP listener with SO_REUSEPORT on {addr}"))?;
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    TcpListener::from_std(std_listener)
        .context("failed to register the TCP listener with the runtime")
}

/// Bind a single TCP listener, for the DNS, TLS or HTTPS server.
#[cfg(not(target_os = "linux"))]
pub async fn bind_tcp(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind TCP listener on {addr}"))
}

/// Create, arm and bind a socket, leaving it ready to hand to `from_raw_fd`.
///
/// One function owns the `close()` on every failure between `socket()` and the
/// transfer of ownership, which is why the option setup is not spelled out at
/// each call site the way it was when only UDP needed it.
#[cfg(target_os = "linux")]
fn bind_raw_socket(
    addr: SocketAddr,
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

    let arm = || -> Result<()> {
        set_reuse_addr(fd)?;
        set_reuse_port(fd)?;
        set_nonblocking(fd)?;
        bind_raw(fd, addr)?;
        // A bound stream socket is not a listener until `listen()` runs, and a
        // datagram socket must not call it: deriving it from the type instead
        // of taking a flag makes forgetting it on the TCP path impossible.
        if sock_type == libc::SOCK_STREAM {
            listen_raw(fd, LISTEN_BACKLOG)?;
        }
        Ok(())
    };

    arm().inspect_err(|_| {
        unsafe { libc::close(fd) };
    })?;
    Ok(fd)
}

// ---------------------------------------------------------------------------
// Linux-only socket calls
// ---------------------------------------------------------------------------

/// Set on both kinds, for different reasons. The TCP bind this replaces set it
/// — mio sets `SO_REUSEADDR` on every non-Windows TCP listener — so dropping it
/// would change restart behaviour rather than only add the overlap. The UDP
/// reuseport path set it too, and it is what lets a restarting process rebind an
/// address its predecessor still holds; the plain `UdpSocket::bind` it now
/// replaces did not, so this is the one behaviour a count of 1 gains.
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

/// `bind()` alone does not make a TCP socket a listener: the bind this path
/// replaces called `listen()` on its way.
#[cfg(target_os = "linux")]
fn listen_raw(fd: libc::c_int, backlog: libc::c_int) -> Result<()> {
    let ret = unsafe { libc::listen(fd, backlog) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error())
            .context(format!("libc::listen(backlog={backlog}) failed"));
    }
    Ok(())
}
