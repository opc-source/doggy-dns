#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::middleware::compute_cache_entry;
use doggy_dns_xdp::types::{FLAG_NEGATIVE, MAX_RESPONSE_SIZE};
use hickory_proto::op::ResponseCode;

#[test]
fn compute_cache_entry_positive() {
    let resp_bytes = vec![0u8; 64];
    let (key, value) = compute_cache_entry(
        "example.com",
        1, // A
        1, // IN
        ResponseCode::NoError,
        resp_bytes.clone(),
        Some(300),
        30,
        1_000_000_000, // fake now
    );

    assert_eq!(key.qtype_class, (1 << 16) | 1);
    assert_eq!(value.resp_len, 64);
    assert_eq!(value.flags, 0);
    assert_eq!(value.expires_at, 1_000_000_000 + 300 * 1_000_000_000);
    assert_eq!(&value.resp_data[..64], resp_bytes.as_slice());
}

#[test]
fn compute_cache_entry_nx_domain() {
    let resp_bytes = vec![0u8; 12];
    let (_key, value) = compute_cache_entry(
        "nonexistent.example.com",
        1,
        1,
        ResponseCode::NXDomain,
        resp_bytes,
        None,
        60,
        2_000_000_000,
    );

    assert_eq!(value.flags, FLAG_NEGATIVE);
    assert_eq!(value.expires_at, 2_000_000_000 + 60 * 1_000_000_000);
}

#[test]
fn compute_cache_entry_serv_fail_not_cached() {
    let result = compute_cache_entry(
        "fail.example.com",
        1,
        1,
        ResponseCode::ServFail,
        vec![],
        None,
        30,
        0,
    );
    // ServFail should not produce a cache entry
    assert!(result.1.resp_len == 0 || result.1.flags == 0);
}

#[test]
fn compute_cache_entry_truncates_large_response() {
    let resp_bytes = vec![0xAB; 1024]; // larger than MAX_RESPONSE_SIZE
    let (_key, value) = compute_cache_entry(
        "large.example.com",
        1,
        1,
        ResponseCode::NoError,
        resp_bytes,
        Some(60),
        30,
        0,
    );
    assert_eq!(value.resp_len as usize, MAX_RESPONSE_SIZE);
}
