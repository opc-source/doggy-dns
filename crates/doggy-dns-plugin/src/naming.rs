use hickory_proto::op::ResponseCode;
use hickory_proto::rr::RecordType;
use hickory_server::net::xfer::Protocol;
use std::borrow::Cow;

/// The lowercase transport name `network.protocol.name` carries.
///
/// UDP and TCP — everything a plain DNS listener sees — answer with a static
/// string; the feature-gated transports keep the derived spelling.
pub fn protocol_name(protocol: Protocol) -> Cow<'static, str> {
    match protocol {
        Protocol::Udp => Cow::Borrowed("udp"),
        Protocol::Tcp => Cow::Borrowed("tcp"),
        _ => Cow::Owned(format!("{protocol:?}").to_ascii_lowercase()),
    }
}

/// The uppercase mnemonic `dns.response.code` carries.
///
/// The codes a query actually ends in are static, so the common paths format
/// nothing; anything rarer keeps the derived `Debug` spelling.
pub fn response_code_name(code: ResponseCode) -> Cow<'static, str> {
    let name = match code {
        ResponseCode::NoError => "NOERROR",
        ResponseCode::FormErr => "FORMERR",
        ResponseCode::ServFail => "SERVFAIL",
        ResponseCode::NXDomain => "NXDOMAIN",
        ResponseCode::NotImp => "NOTIMP",
        ResponseCode::Refused => "REFUSED",
        _ => return Cow::Owned(format!("{code:?}").to_ascii_uppercase()),
    };
    Cow::Borrowed(name)
}

/// The record-type spelling `dns.question.type` carries.
///
/// Every named variant except `ZERO` is static, so the types a query actually
/// asks for format nothing; `ZERO` — a named variant no query asks for — keeps
/// the derived `Debug` spelling alongside the numeric variants
/// (`Unknown(65000)`).
pub fn record_type_name(record_type: RecordType) -> Cow<'static, str> {
    let name = match record_type {
        RecordType::A => "A",
        RecordType::AAAA => "AAAA",
        RecordType::ANAME => "ANAME",
        RecordType::ANY => "ANY",
        RecordType::AXFR => "AXFR",
        RecordType::CAA => "CAA",
        RecordType::CDS => "CDS",
        RecordType::CDNSKEY => "CDNSKEY",
        RecordType::CERT => "CERT",
        RecordType::CNAME => "CNAME",
        RecordType::DNAME => "DNAME",
        RecordType::CSYNC => "CSYNC",
        RecordType::DNSKEY => "DNSKEY",
        RecordType::DS => "DS",
        RecordType::HINFO => "HINFO",
        RecordType::HTTPS => "HTTPS",
        RecordType::IXFR => "IXFR",
        RecordType::KEY => "KEY",
        RecordType::MX => "MX",
        RecordType::NAPTR => "NAPTR",
        RecordType::NS => "NS",
        RecordType::NSEC => "NSEC",
        RecordType::NSEC3 => "NSEC3",
        RecordType::NSEC3PARAM => "NSEC3PARAM",
        RecordType::NULL => "NULL",
        RecordType::OPENPGPKEY => "OPENPGPKEY",
        RecordType::OPT => "OPT",
        RecordType::PTR => "PTR",
        RecordType::RRSIG => "RRSIG",
        RecordType::SIG => "SIG",
        RecordType::SMIMEA => "SMIMEA",
        RecordType::SOA => "SOA",
        RecordType::SRV => "SRV",
        RecordType::SSHFP => "SSHFP",
        RecordType::SVCB => "SVCB",
        RecordType::TLSA => "TLSA",
        RecordType::TSIG => "TSIG",
        RecordType::TXT => "TXT",
        _ => return Cow::Owned(format!("{record_type:?}")),
    };
    Cow::Borrowed(name)
}
