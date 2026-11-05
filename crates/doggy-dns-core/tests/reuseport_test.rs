#![allow(clippy::disallowed_methods)]

use doggy_dns_core::reuseport;
use std::net::{SocketAddr, TcpListener, UdpSocket};

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, RawFd};

#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Ask the kernel what a socket was armed with, straight from the fd: the
/// `SO_REUSEPORT` group is decided at `bind()` time, so reading the option back
/// is the only proof that it was set before the bind rather than after it.
#[cfg(target_os = "linux")]
fn sockopt(fd: RawFd, name: libc::c_int) -> libc::c_int {
    let mut val: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            &mut val as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    assert_eq!(
        ret,
        0,
        "getsockopt(name={name}) failed: {}",
        std::io::Error::last_os_error()
    );
    val
}

/// A port nobody holds: bind the ephemeral probe, read it back, release it.
/// The reuse tests need a fixed address, because the second socket has to aim
/// at exactly what the first one took.
fn free_udp_port() -> u16 {
    let probe: SocketAddr = "127.0.0.1:0".parse().expect("loopback literal");
    let socket = UdpSocket::bind(probe).expect("UDP probe bind");
    socket.local_addr().expect("UDP probe address").port()
}

fn free_tcp_port() -> u16 {
    let probe: SocketAddr = "127.0.0.1:0".parse().expect("loopback literal");
    let socket = TcpListener::bind(probe).expect("TCP probe bind");
    socket.local_addr().expect("TCP probe address").port()
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bind_udp_sets_the_reuse_options() {
    let socket = reuseport::bind_udp(loopback(free_udp_port()))
        .await
        .expect("UDP bind");
    let fd = socket.as_raw_fd();
    assert_eq!(sockopt(fd, libc::SO_REUSEPORT), 1, "SO_REUSEPORT");
    assert_eq!(sockopt(fd, libc::SO_REUSEADDR), 1, "SO_REUSEADDR");
}

/// The behaviour a restart depends on: a second socket joins the group while
/// the first is still open. Without the option that bind is EADDRINUSE — which
/// is also why the plain socket below is expected to fail.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn bind_udp_accepts_a_second_socket_on_the_same_address() {
    let addr = loopback(free_udp_port());
    let first = reuseport::bind_udp(addr).await.expect("first UDP bind");
    let second = reuseport::bind_udp(addr)
        .await
        .expect("second UDP bind should overlap the first");

    assert_eq!(first.local_addr().expect("first address"), addr);
    assert_eq!(second.local_addr().expect("second address"), addr);

    let plain = UdpSocket::bind(addr);
    assert!(
        plain.is_err(),
        "a socket outside the group must still be refused the address"
    );
}

/// A plain `bind()` leaves a TCP socket passive and `accept()` on it fails
/// until `listen()` runs. This path calls `listen()` itself rather than let
/// `TcpListener::bind` do it, so the state is asserted instead of assumed to
/// have carried over — and the backlog it picks has to be big enough to accept.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn bind_tcp_leaves_a_listener_that_accepts() {
    let addr = loopback(free_tcp_port());
    let listener = reuseport::bind_tcp(addr).await.expect("TCP bind");

    let fd = listener.as_raw_fd();
    assert_eq!(sockopt(fd, libc::SO_ACCEPTCONN), 1, "socket is listening");
    assert_eq!(sockopt(fd, libc::SO_REUSEPORT), 1, "SO_REUSEPORT");

    // Deliberately one listener only: a reuseport group hands a new connection
    // to a single member, so naming the socket that accepts needs a group of one.
    let mut client = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect to the listener");
    let (mut accepted, _) = listener.accept().await.expect("accept");
    client.write_all(b"hi").await.expect("client write");
    let mut buf = [0u8; 2];
    accepted.read_exact(&mut buf).await.expect("accepted read");
    assert_eq!(&buf, b"hi");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn bind_tcp_accepts_a_second_listener_on_the_same_address() {
    let addr = loopback(free_tcp_port());
    let first = reuseport::bind_tcp(addr).await.expect("first TCP bind");
    let second = reuseport::bind_tcp(addr)
        .await
        .expect("second TCP bind should overlap the first");

    assert_eq!(first.local_addr().expect("first address"), addr);
    assert_eq!(second.local_addr().expect("second address"), addr);

    let plain = TcpListener::bind(addr);
    assert!(
        plain.is_err(),
        "a listener outside the group must still be refused the address"
    );
}

/// Both bind functions are also the non-Linux path, where they delegate to the
/// std bind. Kept unconditional so that branch cannot rot: two sockets simply
/// cannot share one address there, which is what the config rule enforces.
#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn bind_falls_back_to_a_plain_bind_without_the_overlap() {
    let udp_addr = loopback(free_udp_port());
    let udp = reuseport::bind_udp(udp_addr).await.expect("UDP bind");
    assert_eq!(udp.local_addr().expect("UDP address"), udp_addr);
    assert!(
        reuseport::bind_udp(udp_addr).await.is_err(),
        "a second UDP socket on one address is impossible without the option"
    );
    drop(udp);

    let tcp_addr = loopback(free_tcp_port());
    let tcp = reuseport::bind_tcp(tcp_addr).await.expect("TCP bind");
    assert_eq!(tcp.local_addr().expect("TCP address"), tcp_addr);
}
