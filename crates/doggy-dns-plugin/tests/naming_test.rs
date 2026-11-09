#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::naming::{protocol_name, record_type_name, response_code_name};
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::RecordType;
use hickory_server::net::xfer::Protocol;
use std::borrow::Cow;

#[test]
fn plain_transports_have_static_names() {
    assert_eq!(protocol_name(Protocol::Udp), "udp");
    assert_eq!(protocol_name(Protocol::Tcp), "tcp");
}

#[test]
fn common_response_codes_are_upper_case_mnemonics() {
    assert_eq!(response_code_name(ResponseCode::NoError), "NOERROR");
    assert_eq!(response_code_name(ResponseCode::NXDomain), "NXDOMAIN");
    assert_eq!(response_code_name(ResponseCode::ServFail), "SERVFAIL");
    assert_eq!(response_code_name(ResponseCode::FormErr), "FORMERR");
}

/// Every named `RecordType` variant the mapper's static arms cover, in the
/// enum's declaration order; `ZERO` is pinned separately as a fallback because
/// no arm covers it. Each name must match what `Debug` prints.
const NAMED_RECORD_TYPES: [RecordType; 38] = [
    RecordType::A,
    RecordType::AAAA,
    RecordType::ANAME,
    RecordType::ANY,
    RecordType::AXFR,
    RecordType::CAA,
    RecordType::CDS,
    RecordType::CDNSKEY,
    RecordType::CERT,
    RecordType::CNAME,
    RecordType::DNAME,
    RecordType::CSYNC,
    RecordType::DNSKEY,
    RecordType::DS,
    RecordType::HINFO,
    RecordType::HTTPS,
    RecordType::IXFR,
    RecordType::KEY,
    RecordType::MX,
    RecordType::NAPTR,
    RecordType::NS,
    RecordType::NSEC,
    RecordType::NSEC3,
    RecordType::NSEC3PARAM,
    RecordType::NULL,
    RecordType::OPENPGPKEY,
    RecordType::OPT,
    RecordType::PTR,
    RecordType::RRSIG,
    RecordType::SIG,
    RecordType::SMIMEA,
    RecordType::SOA,
    RecordType::SRV,
    RecordType::SSHFP,
    RecordType::SVCB,
    RecordType::TLSA,
    RecordType::TSIG,
    RecordType::TXT,
];

#[test]
fn named_record_types_match_the_debug_spelling() {
    for record_type in NAMED_RECORD_TYPES {
        let name = record_type_name(record_type);
        assert!(
            matches!(name, Cow::Borrowed(_)),
            "static arm for {record_type:?} must not allocate"
        );
        assert_eq!(
            name,
            format!("{record_type:?}"),
            "static arm for {record_type:?} must match the Debug spelling"
        );
    }
}

#[test]
fn numeric_record_types_keep_the_debug_spelling() {
    let unknown = record_type_name(RecordType::Unknown(65000));
    assert!(
        matches!(unknown, Cow::Owned(_)),
        "the numeric fallback allocates"
    );
    assert_eq!(unknown, "Unknown(65000)");

    let zero = record_type_name(RecordType::ZERO);
    assert!(
        matches!(zero, Cow::Owned(_)),
        "the numeric fallback allocates"
    );
    assert_eq!(zero, "ZERO");
}
