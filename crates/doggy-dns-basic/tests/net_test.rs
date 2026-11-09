#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::net::{parse_upstream, socket_addr};
use std::net::SocketAddr;

#[test]
fn parses_ipv4_listen_address() {
    assert_eq!(
        socket_addr("0.0.0.0", 15353).unwrap(),
        "0.0.0.0:15353".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn parses_ipv6_listen_addresses() {
    assert_eq!(
        socket_addr("::", 15353).unwrap(),
        "[::]:15353".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(
        socket_addr("::1", 53).unwrap(),
        "[::1]:53".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn accepts_bracketed_ipv6() {
    assert_eq!(
        socket_addr("[::1]", 443).unwrap(),
        "[::1]:443".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn rejects_hostnames_and_garbage() {
    assert!(socket_addr("localhost", 53).is_err());
    assert!(socket_addr("", 53).is_err());
    assert!(socket_addr("[::1", 53).is_err());
}

#[test]
fn trims_surrounding_whitespace() {
    assert_eq!(
        socket_addr(" 127.0.0.1 ", 5353).unwrap(),
        "127.0.0.1:5353".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn upstream_defaults_to_port_53() {
    assert_eq!(
        parse_upstream("8.8.8.8").unwrap(),
        "8.8.8.8:53".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(
        parse_upstream("2001:db8::1").unwrap(),
        "[2001:db8::1]:53".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn upstream_keeps_an_explicit_port() {
    assert_eq!(
        parse_upstream("8.8.8.8:5353").unwrap(),
        "8.8.8.8:5353".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(
        parse_upstream("[2001:db8::1]:5353").unwrap(),
        "[2001:db8::1]:5353".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn upstream_rejects_hostnames() {
    assert!(parse_upstream("dns.google").is_err());
    assert!(parse_upstream("").is_err());
}
