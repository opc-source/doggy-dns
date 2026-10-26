#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::RequestOutcome;
use hickory_proto::op::ResponseCode;

#[test]
fn default_outcome_has_no_response_bytes() {
    let outcome = RequestOutcome::success("test".to_string(), 1, None, None);
    assert!(outcome.response_bytes.is_none());
    assert!(outcome.min_answer_ttl.is_none());
}

#[test]
fn outcome_carries_response_bytes() {
    let bytes = vec![0u8; 64];
    let outcome = RequestOutcome::success("forward".to_string(), 2, Some(bytes.clone()), Some(300));
    assert_eq!(outcome.response_bytes.as_deref(), Some(bytes.as_slice()));
    assert_eq!(outcome.min_answer_ttl, Some(300));
}

#[test]
fn nxdomain_outcome_has_zero_ttl_and_no_bytes() {
    let outcome = RequestOutcome::nxdomain(Some(vec![0u8; 12]));
    assert_eq!(outcome.response_code, ResponseCode::NXDomain);
    assert_eq!(outcome.answers, 0);
    assert!(outcome.min_answer_ttl.is_none());
}
