#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::watcher::{
    CachedInstance, FailureBackoff, NACOS_FAILURE_BACKOFF, NACOS_FAILURE_THRESHOLD, ServiceFilter,
    usable_instances,
};
use nacos_sdk::api::naming::ServiceInstance;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

fn make_service_instance(ip: &str, port: i32, healthy: bool, enabled: bool) -> ServiceInstance {
    ServiceInstance {
        instance_id: None,
        ip: ip.to_string(),
        port,
        weight: 1.0,
        healthy,
        enabled,
        ephemeral: true,
        cluster_name: None,
        service_name: None,
        metadata: HashMap::new(),
    }
}

/// The client keeps instances this resolver cannot use; they are dropped when a
/// query reads the list.
#[test]
fn usable_instances_keeps_healthy_enabled_addresses() {
    let instances = vec![
        make_service_instance("10.0.0.1", 8080, true, true),
        make_service_instance("10.0.0.2", 8080, false, true), // unhealthy
        make_service_instance("10.0.0.3", 8080, true, false), // disabled
        make_service_instance("not-an-ip", 8080, true, true),
    ];

    let usable = usable_instances(&instances);

    assert_eq!(usable.len(), 1);
    assert_eq!(usable[0].ip, IpAddr::from([10, 0, 0, 1]));
    assert_eq!(usable[0].port, 8080);
}

#[test]
fn cached_instance_parses_the_address_once() {
    let si = make_service_instance("192.168.1.100", 9090, true, true);
    let cached = CachedInstance::from_service_instance(&si).expect("valid IPv4");
    assert_eq!(cached.ip, IpAddr::from([192, 168, 1, 100]));
    assert_eq!(cached.port, 9090);
    assert!(cached.healthy);
}

#[test]
fn cached_instance_rejects_a_non_address() {
    let si = make_service_instance("not-an-ip", 9090, true, true);
    assert!(
        CachedInstance::from_service_instance(&si).is_none(),
        "an unparsable address must not reach the cache"
    );
}

/// The built-in pattern is the application-name rule: word characters, `-`, no
/// dots. A dot means the registration is not a bare application name (a
/// `com.foo.Bar` style identifier, or the `group@@service` form some Nacos
/// versions return), and those are exactly the entries a deployment does not
/// want pushed to it.
#[test]
fn default_filter_selects_bare_application_names() {
    let filter = ServiceFilter::from_config(None).expect("the built-in pattern must compile");

    for selected in ["order-service", "ORDER_svc-1", "a1"] {
        assert!(
            filter.is_selected(selected),
            "{selected} is a bare application name and must be subscribed"
        );
    }

    for rejected in [
        "com.foo.Bar",
        "order.service",
        "DEFAULT_GROUP@@order",
        "服务名",
        "order service",
        "",
    ] {
        assert!(
            !filter.is_selected(rejected),
            "{rejected:?} is not a bare application name and must not be subscribed"
        );
    }
}

/// A configured pattern is wrapped in whole-name anchors, so `user-service-\d+`
/// cannot silently subscribe `my-user-service-1` through a substring match. An
/// author who already wrote anchors keeps working.
#[test]
fn configured_filter_matches_the_whole_name() {
    let filter = ServiceFilter::from_config(Some(r"user-service-\d+")).expect("valid pattern");

    assert!(filter.is_selected("user-service-12"));
    assert!(
        !filter.is_selected("my-user-service-1"),
        "an unanchored substring match would subscribe a service the operator did not name"
    );
    assert!(!filter.is_selected("user-service-1x"));

    let pre_anchored = ServiceFilter::from_config(Some(r"^order$")).expect("valid pattern");
    assert!(pre_anchored.is_selected("order"));
    assert!(!pre_anchored.is_selected("order-2"));
}

/// Matching is case-sensitive and nothing lowercases the configured pattern. The
/// production caller hands `is_selected` a `LowerName` label, so a mixed-case
/// name only reaches here from a test.
#[test]
fn filter_matching_is_case_sensitive() {
    let filter = ServiceFilter::from_config(Some("order")).expect("valid pattern");

    assert!(filter.is_selected("order"));
    assert!(
        !filter.is_selected("ORDER"),
        "a lowercase pattern must not silently select a differently-cased name"
    );
}

/// Characterisation of the assumption behind [`DEFAULT_SERVICE_PATTERN`] being
/// spelled as an ASCII class: with the regex crate's default `unicode` feature,
/// a `\w` an operator writes here is Unicode-aware, so it does select a CJK
/// registration. The built-in default must not inherit that breadth.
#[test]
fn user_written_word_class_is_unicode_aware() {
    let filter = ServiceFilter::from_config(Some(r"\w+")).expect("valid pattern");
    let default = ServiceFilter::from_config(None).expect("the built-in pattern must compile");

    assert!(
        filter.is_selected("服务名"),
        r#""\w" in a configured pattern matches Unicode word characters"#
    );
    assert!(
        !default.is_selected("服务名"),
        "the built-in rule is ASCII-only regardless of how that could be written"
    );
}

/// A pattern that does not compile is a configuration error naming the field and
/// the text, so the operator sees which plugin line to fix.
#[test]
fn invalid_pattern_is_reported_with_the_pattern() {
    let error = ServiceFilter::from_config(Some("order(")).expect_err("unbalanced group must fail");
    let message = error.to_string();

    assert!(
        message.contains("service_pattern"),
        "the error must name the config field, got: {message}"
    );
    assert!(
        message.contains("order("),
        "the error must quote the rejected pattern, got: {message}"
    );
}

// The clock is a parameter of every call below, so the window's boundaries are
// asserted exactly rather than slept through.

/// One failure is a query, two are a bad minute, three are an outage worth
/// stopping for - fewer and a flaky link would shut the remote down.
#[test]
fn the_window_opens_on_the_third_consecutive_failure() {
    let backoff = FailureBackoff::new();
    let now = 1_700_000_000;

    assert!(
        !backoff.record_failure(now),
        "one failure must not open the window"
    );
    assert!(
        !backoff.record_failure(now),
        "two failures must not open the window"
    );
    assert!(
        backoff.record_failure(now),
        "the third consecutive failure must open the window"
    );
    assert!(
        backoff.is_open(now),
        "an opened window covers its own instant"
    );
}

/// A success restarts the streak, so intermittent failures that still deliver
/// answers never accumulate into a window.
#[test]
fn a_success_restarts_the_failure_streak() {
    let backoff = FailureBackoff::new();
    let now = 1_700_000_000;

    backoff.record_failure(now);
    backoff.record_failure(now);
    backoff.record_success();

    assert!(
        !backoff.record_failure(now),
        "the streak restarted, so this failure is only the first"
    );
    assert!(!backoff.is_open(now), "no window after a restart");
}

/// The window holds for the whole backoff and lifts at its end, which is when
/// the next query is allowed to call Nacos again.
#[test]
fn the_window_lifts_when_the_backoff_expires() {
    let backoff = FailureBackoff::new();
    let opened_at = 1_700_000_000;
    let backoff_secs = i64::try_from(NACOS_FAILURE_BACKOFF.as_secs()).expect("backoff in seconds");
    backoff.record_failure(opened_at);
    backoff.record_failure(opened_at);
    assert!(backoff.record_failure(opened_at));

    assert!(
        backoff.is_open(opened_at + backoff_secs - 1),
        "the last second of the window must still hold"
    );
    assert!(
        !backoff.is_open(opened_at + backoff_secs),
        "the window must lift at its end, or nothing retries Nacos"
    );
}

/// After the window lifts, the next failure opens it again on its own: the
/// streak is already proven, and making the remote pay the threshold all over
/// again would spend three more timeouts on a dead server.
#[test]
fn one_failure_after_the_window_reopens_it() {
    let backoff = FailureBackoff::new();
    let opened_at = 1_700_000_000;
    let backoff_secs = i64::try_from(NACOS_FAILURE_BACKOFF.as_secs()).expect("backoff in seconds");
    backoff.record_failure(opened_at);
    backoff.record_failure(opened_at);
    backoff.record_failure(opened_at);

    let expired = opened_at + backoff_secs;
    assert!(!backoff.is_open(expired), "the probe is allowed again");
    assert!(
        backoff.record_failure(expired),
        "one failure after a proven outage must reopen the window"
    );
    assert!(backoff.is_open(expired));
}

/// The threshold is the documented number, not an accident of the tests above.
#[test]
fn the_failure_threshold_is_three() {
    assert_eq!(NACOS_FAILURE_THRESHOLD, 3);
    assert_eq!(NACOS_FAILURE_BACKOFF, Duration::from_secs(30));
}

/// A confirmation that got through proves the remote is answering, so the
/// window it was opened under is no longer true. Without this, a query that was
/// already in flight when the window opened would leave the window holding for
/// its full backoff against a server that is demonstrably alive.
#[test]
fn a_success_while_the_window_is_open_lifts_it() {
    let backoff = FailureBackoff::new();
    let now = 1_700_000_000;

    backoff.record_failure(now);
    backoff.record_failure(now);
    assert!(backoff.record_failure(now));
    assert!(backoff.is_open(now), "the window is open");

    backoff.record_success();

    assert!(
        !backoff.is_open(now),
        "a confirmed answer must end the window, not wait for its backoff"
    );
    assert!(
        !backoff.record_failure(now),
        "the streak restarted too, so the next failure is only the first"
    );
}

/// The window is measured in wall-clock seconds, so the clock can step
/// backwards under it (an NTP correction). The difference has to be read as
/// "not open" rather than saturating to zero: a negative elapsed time is a
/// window that has not started yet, and holding it open for the whole jump
/// would refuse new names for minutes on a clock skew, not on an outage.
#[test]
fn a_backwards_clock_step_lifts_the_window() {
    let backoff = FailureBackoff::new();
    let opened_at = 1_700_000_000;

    backoff.record_failure(opened_at);
    backoff.record_failure(opened_at);
    assert!(backoff.record_failure(opened_at));
    assert!(backoff.is_open(opened_at), "the window is open");

    assert!(
        !backoff.is_open(opened_at - 1),
        "a clock that stepped behind the window's start must not hold it open"
    );
    assert!(
        !backoff.is_open(opened_at - 1_000_000),
        "a large step back must lift the window, not extend it by the whole jump"
    );
}

/// The window ends at its own instant's far edge, and a clock behind the start
/// is not inside it: only a non-negative elapsed time counts.
#[test]
fn the_window_covers_exactly_its_backoff_from_its_start() {
    let backoff = FailureBackoff::new();
    let opened_at = 1_700_000_000;
    let backoff_secs = i64::try_from(NACOS_FAILURE_BACKOFF.as_secs()).expect("backoff in seconds");

    backoff.record_failure(opened_at);
    backoff.record_failure(opened_at);
    assert!(backoff.record_failure(opened_at));

    assert!(backoff.is_open(opened_at), "the first second counts");
    assert!(
        backoff.is_open(opened_at + backoff_secs - 1),
        "the last second counts"
    );
    assert!(
        !backoff.is_open(opened_at + backoff_secs),
        "the window lifts at its end"
    );
}
