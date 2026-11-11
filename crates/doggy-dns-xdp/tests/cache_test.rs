#![allow(clippy::disallowed_methods)]

use doggy_dns_xdp::cache::{fnv1a_64, make_cache_key};
use doggy_dns_xdp::types::CacheValue;
use doggy_dns_xdp_abi::{
    FLAG_NEGATIVE, FNV_OFFSET_BASIS, MAX_RESPONSE_SIZE, fnv1a_word, lower_word,
};
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
    // A lone byte is one zero-padded word, so the classic byte-wise FNV-1a
    // constant still applies: FNV-1a of "a" = 0xaf63dc4c8601ec8c.
    assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
}

#[test]
fn fnv1a_hashes_eight_byte_words() {
    // One little-endian u64 word: byte-wise FNV-1a of the same input would
    // run eight multiply steps and produce a different value.
    assert_eq!(fnv1a_64(b"abcdefgh"), 0x3919eeb00ee1e68c);
}

#[test]
fn fnv1a_pads_to_whole_words() {
    // Nine bytes are two words: 0x6867666564636261, then 0x69 with the
    // remaining seven bytes zero-padded.
    assert_eq!(fnv1a_64(b"abcdefghi"), 0xe8f77a2949db571f);

    // Padding is to the next whole word, not to a fixed length: four bytes
    // and the same four bytes followed by four zeros hash identically, while
    // eight explicit zeros are a second, extra word.
    assert_eq!(fnv1a_64(b"abcd"), fnv1a_64(b"abcd\0\0\0\0"));
    assert_ne!(fnv1a_64(b"abcd"), fnv1a_64(b"abcd\0\0\0\0\0\0\0\0"));
}

#[test]
fn fnv1a_case_sensitive() {
    let lower = fnv1a_64(b"example.com");
    let upper = fnv1a_64(b"EXAMPLE.COM");
    assert_ne!(lower, upper);
}

#[test]
fn fnv1a_pads_every_partial_word() {
    let bytes = b"abcdefghi";
    // A short tail is the same word as those bytes zero-padded to eight, so
    // every tail length 1..=8 must fold identically.
    for n in 1..=8 {
        let mut padded = bytes[..n].to_vec();
        padded.resize(8, 0);
        assert_eq!(
            fnv1a_64(&bytes[..n]),
            fnv1a_64(&padded),
            "tail of {n} bytes"
        );
    }
    // Nine bytes are two words: the ninth byte cannot be folded into the
    // first word, so the eight-byte prefix hashes differently on its own.
    assert_ne!(fnv1a_64(&bytes[..8]), fnv1a_64(&bytes[..9]));
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

#[test]
fn make_cache_key_hashes_wire_form_at_max_length() {
    // The widest name hickory accepts: four labels, 255 wire bytes including
    // the length prefixes and the root terminator. This is the exact input
    // that fills `make_cache_key`'s uninitialised stack buffer.
    let long = "a".repeat(63);
    let tail = "b".repeat(61);
    let name = name(&format!("{long}.{long}.{long}.{tail}."));
    let labels: [&[u8]; 4] = [
        long.as_bytes(),
        long.as_bytes(),
        long.as_bytes(),
        tail.as_bytes(),
    ];
    let wire = wire_qname(&labels);
    assert_eq!(wire.len(), 255);
    assert_eq!(make_cache_key(&name, 1, 1).name_hash, fnv1a_64(&wire));
}

#[test]
fn lower_word_matches_byte_wise_for_every_byte() {
    for b in 0..=u8::MAX {
        assert_eq!(
            lower_word(b as u64),
            b.to_ascii_lowercase() as u64,
            "byte {b:#04x}"
        );
    }
}

#[test]
fn lower_word_folds_every_byte_of_a_mixed_word() {
    // '@' (0x40) and '[' (0x5B) sit just outside 'A'..='Z'; 0xFF already has
    // the high bit set and must pass through untouched.
    let bytes = [b'A', b'z', b'0', b'[', b'@', b'Z', b'a', 0xFF];
    let word = u64::from_le_bytes(bytes);
    let expected = u64::from_le_bytes(bytes.map(|b| b.to_ascii_lowercase()));
    assert_eq!(lower_word(word), expected);
}

#[test]
fn lower_word_leaves_high_bytes_alone() {
    let word = u64::from_le_bytes([0x80, 0x9F, 0xC0, 0xDF, 0xE0, 0xEF, 0xF0, 0xFF]);
    assert_eq!(lower_word(word), word);
}

#[test]
fn fnv1a_word_matches_classic_single_byte() {
    // A lone byte is one zero-padded word: classic FNV-1a of "a".
    assert_eq!(
        fnv1a_word(FNV_OFFSET_BASIS, b'a' as u64),
        0xaf63dc4c8601ec8c
    );
}

#[test]
fn fnv1a_word_chains_to_the_known_eight_byte_vector() {
    let word = u64::from_le_bytes(*b"abcdefgh");
    assert_eq!(fnv1a_word(FNV_OFFSET_BASIS, word), 0x3919eeb00ee1e68c);
}

#[test]
fn cache_value_zeroes_the_tail_beyond_the_payload() {
    let value = CacheValue::new_positive(&[0xAB; 64], 42);
    assert_eq!(&value.resp_data[..64], &[0xAB; 64]);
    assert!(value.resp_data[64..].iter().all(|&b| b == 0));
    assert_eq!(value.resp_len, 64);
    assert_eq!(value.flags, 0);
    assert_eq!(value.expires_at, 42);
}

#[test]
fn cache_value_negative_stamps_the_flag() {
    let value = CacheValue::new_negative(b"\x00\x01", 7);
    assert_eq!(value.resp_len, 2);
    assert_eq!(value.flags, FLAG_NEGATIVE);
    assert!(value.resp_data[2..].iter().all(|&b| b == 0));
}

#[test]
fn cache_value_truncates_oversized_payloads() {
    let resp = vec![0xCD; MAX_RESPONSE_SIZE + 16];
    let value = CacheValue::new_positive(&resp, 1);
    assert_eq!(value.resp_len as usize, MAX_RESPONSE_SIZE);
    assert_eq!(&value.resp_data[..], &resp[..MAX_RESPONSE_SIZE]);
}
