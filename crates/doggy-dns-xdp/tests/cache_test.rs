#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::cache::fnv1a_64;

#[test]
fn fnv1a_empty_string() {
    assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
}

#[test]
fn fnv1a_known_value() {
    // FNV-1a of "a" = 0xaf63dc4c8601ec8c
    assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
}

#[test]
fn fnv1a_case_sensitive() {
    let lower = fnv1a_64(b"example.com");
    let upper = fnv1a_64(b"EXAMPLE.COM");
    assert_ne!(lower, upper);
}

#[test]
fn fnv1a_deterministic() {
    let input = b"svc-1.nacos.default.doggy.test";
    assert_eq!(fnv1a_64(input), fnv1a_64(input));
}
