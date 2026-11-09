use hickory_proto::op::ResponseCode;
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
