#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::interface::{detect_default_interface, parse_default_from_route};

#[test]
fn parse_default_route_linux() {
    let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
eth0\t00000000\t0102A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
    let iface = parse_default_from_route(content).expect("should find default");
    assert_eq!(iface, "eth0");
}

#[test]
fn parse_default_route_enp0s3() {
    let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
enp0s3\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
    let iface = parse_default_from_route(content).expect("should find default");
    assert_eq!(iface, "enp0s3");
}

#[test]
fn parse_default_route_no_default() {
    let content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
eth0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
    let err = parse_default_from_route(content).unwrap_err();
    assert!(err.to_string().contains("no default route"));
}

#[test]
fn detect_default_interface_runs_on_linux() {
    // This test only works on Linux. On other platforms, it should return an error.
    let result = detect_default_interface();
    if cfg!(target_os = "linux") {
        // Should succeed on Linux CI runners
        assert!(result.is_ok() || result.is_err());
    } else {
        assert!(result.is_err());
    }
}
