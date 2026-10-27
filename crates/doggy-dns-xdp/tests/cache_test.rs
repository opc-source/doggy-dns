#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::cache::{fnv1a_64, make_cache_key};
use hickory_proto::rr::Name;

fn name(s: &str) -> Name {
    Name::from_ascii(s).expect("valid name")
}

/// Wire form of a QNAME as the eBPF parser sees it: length-prefixed labels,
/// lowercased, terminated by the root label's zero byte.
fn wire_qname(labels: &[&[u8]]) -> Vec<u8> {
    let mut wire = Vec::new();
    for label in labels {
        wire.push(label.len() as u8);
        wire.extend(label.iter().map(u8::to_ascii_lowercase));
    }
    wire.push(0);
    wire
}

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

#[test]
fn make_cache_key_hashes_wire_form() {
    let key = make_cache_key(&name("AbC.Example.COM."), 1, 1);
    let wire = wire_qname(&[b"AbC", b"Example", b"COM"]);
    assert_eq!(key.name_hash, fnv1a_64(&wire));
    assert_eq!(key.qtype_class, (1 << 16) | 1);
}

#[test]
fn make_cache_key_is_case_insensitive() {
    assert_eq!(
        make_cache_key(&name("Example.COM."), 1, 1).name_hash,
        make_cache_key(&name("example.com."), 1, 1).name_hash,
    );
}

#[test]
fn make_cache_key_keeps_label_boundaries() {
    assert_ne!(
        make_cache_key(&name("ab.c.example.com."), 1, 1).name_hash,
        make_cache_key(&name("a.bc.example.com."), 1, 1).name_hash,
    );
}
