#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::bounded::{CallFailure, bounded_call};
use std::future::pending;
use std::time::Duration;

#[tokio::test]
async fn a_value_passes_through() {
    let answer = bounded_call(Duration::from_secs(1), async { Ok::<_, ()>(42) })
        .await
        .expect("call answers");
    assert_eq!(answer, 42);
}

#[tokio::test]
async fn the_calls_error_maps_to_sdk() {
    let failure = bounded_call(Duration::from_secs(1), async { Err::<(), _>("boom") })
        .await
        .expect_err("call fails");
    assert!(matches!(failure, CallFailure::Sdk("boom")));
}

#[tokio::test]
async fn a_hang_maps_to_timeout() {
    let failure = bounded_call(Duration::from_millis(10), pending::<Result<(), ()>>())
        .await
        .expect_err("call hangs");
    assert!(matches!(failure, CallFailure::Timeout));
}
