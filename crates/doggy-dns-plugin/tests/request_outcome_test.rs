#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::RequestOutcome;
use hickory_proto::op::ResponseCode;

#[test]
fn default_outcome_has_no_response_bytes() {
    let outcome = RequestOutcome {
        response_code: ResponseCode::NoError,
        authority: Some("test".to_string()),
        answers: 1,
        response_bytes: None,
        min_answer_ttl: None,
    };
    assert!(outcome.response_bytes.is_none());
    assert!(outcome.min_answer_ttl.is_none());
}

#[test]
fn outcome_carries_response_bytes() {
    let bytes = vec![0u8; 64];
    let outcome = RequestOutcome {
        response_code: ResponseCode::NoError,
        authority: Some("forward".to_string()),
        answers: 2,
        response_bytes: Some(bytes.clone()),
        min_answer_ttl: Some(300),
    };
    assert_eq!(outcome.response_bytes.as_deref(), Some(bytes.as_slice()));
    assert_eq!(outcome.min_answer_ttl, Some(300));
}

#[test]
fn nxdomain_outcome_has_zero_ttl_and_no_bytes() {
    let outcome = RequestOutcome {
        response_code: ResponseCode::NXDomain,
        authority: None,
        answers: 0,
        response_bytes: Some(vec![0u8; 12]),
        min_answer_ttl: None,
    };
    assert_eq!(outcome.response_code, ResponseCode::NXDomain);
    assert_eq!(outcome.answers, 0);
    assert!(outcome.min_answer_ttl.is_none());
}
