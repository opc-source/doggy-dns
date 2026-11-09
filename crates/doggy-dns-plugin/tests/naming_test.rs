#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::naming::{protocol_name, response_code_name};
use hickory_proto::op::ResponseCode;
use hickory_server::net::xfer::Protocol;

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
