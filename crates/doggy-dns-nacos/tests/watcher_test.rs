#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::authority::CatalogAnswer;
use doggy_dns_nacos::watcher::{
    CachedInstance, FailureBackoff, NACOS_CACHE_READ_TIMEOUT, NACOS_FAILURE_BACKOFF,
    NACOS_FAILURE_THRESHOLD, NACOS_LOOKUP_TIMEOUT, QueryEntry, ServiceFilter, cache_hit_of,
    entry_of, usable_instances,
};
use nacos_sdk::api::naming::ServiceInstance;
use regex::Regex;
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

/// The built-in rule is a byte-wise shortcut for the anchored
/// `[A-Za-z0-9_-]+` pattern. Pin the two against a reference regex over every
/// ASCII byte and the multi-byte/punctuation shapes a Nacos name can carry, so
/// the shortcut cannot quietly widen or narrow what an operator configured.
#[test]
fn default_filter_matches_the_ascii_class_exactly() {
    let reference = Regex::new("^(?:[A-Za-z0-9_-]+)$").expect("reference pattern");
    let default = ServiceFilter::from_config(None).expect("the built-in rule");
    let configured = ServiceFilter::from_config(Some("[A-Za-z0-9_-]+"))
        .expect("the explicit default pattern compiles");

    let mut cases: Vec<String> = (0u8..=127)
        .map(|byte| char::from(byte).to_string())
        .collect();
    cases.extend([
        "order-service".to_string(),
        "ORDER_svc-1".to_string(),
        "a1".to_string(),
        "com.foo.Bar".to_string(),
        "DEFAULT_GROUP@@order".to_string(),
        "服务名".to_string(),
        "café".to_string(),
        "order service".to_string(),
        String::new(),
        "abc\0def".to_string(),
    ]);

    for case in cases {
        let expected = reference.is_match(&case);
        assert_eq!(
            default.is_selected(&case),
            expected,
            "the built-in rule diverged from the anchored ASCII class on {case:?}"
        );
        assert_eq!(
            configured.is_selected(&case),
            expected,
            "the regex path diverged from the reference on {case:?}"
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

/// Characterisation of why the built-in rule is spelled as explicit ASCII bytes
/// rather than `\w`: with the regex crate's default `unicode` feature, a `\w` an
/// operator writes here is Unicode-aware, so it does select a CJK registration.
/// The built-in default must not inherit that breadth.
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

/// A reply is credited for the whole life of the process, and an outage that
/// comes later does not take it back: this is the bit that says whether the
/// client has ever been proven able to talk to the registry.
///
/// Without it, every name this process has not subscribed would keep asking the
/// server, and an unknown name would pay two round trips for as long as the
/// process lives. With it held forever, a start-up that began in an outage and
/// recovered would go back to refusing the client cache - which is the case the
/// cache-first entry exists to answer.
#[test]
fn a_server_reply_is_credited_for_the_rest_of_the_process() {
    let backoff = FailureBackoff::new();
    let now = 1_700_000_000;

    assert!(
        !backoff.ever_confirmed(),
        "a fresh process has no evidence about the server"
    );

    backoff.record_failure(now);
    backoff.record_failure(now);
    assert!(
        !backoff.ever_confirmed(),
        "a failure is not evidence that the server answers"
    );

    backoff.record_success();
    assert!(backoff.ever_confirmed(), "a reply is");

    backoff.record_failure(now);
    backoff.record_failure(now);
    backoff.record_failure(now);
    assert!(
        backoff.ever_confirmed(),
        "an outage after a reply does not erase the reply"
    );
}

// The entry the query takes is the whole decision, so it is pinned as a table
// rather than inferred from which network call a test happens to reach.

/// A name already in the ledger reads the client cache whatever else is true, and
/// an open window outranks everything a name that is not.
#[test]
fn the_entry_table_picks_the_source_from_three_inputs() {
    let table = [
        // Subscribed: pushes keep the client cache current, so it is the freshest
        // source available and a down registry does not make it worth cutting off.
        (true, false, false, QueryEntry::ClientCache),
        (true, false, true, QueryEntry::ClientCache),
        (true, true, false, QueryEntry::ClientCache),
        (true, true, true, QueryEntry::ClientCache),
        // Before the server has ever replied, a name the client's cache already
        // holds answers without a round trip. This is the start-up outage: the
        // alternative was to refuse a name the previous run left data for, because
        // the ledger only fills from answers this process got. The cache is tried
        // first, not only used, so an empty list still falls through to the server.
        (false, false, false, QueryEntry::CacheFirst),
        // Once the registry has proven it answers, an unledgered name is asked of it
        // first: an entry loaded from disk is not evidence about a name this process
        // has never confirmed, and serving it would hold the previous run's addresses
        // behind a live server.
        (false, true, false, QueryEntry::ServerFirst),
        // Inside the window the cache is the only source, and a name it does not hold
        // is refused rather than paid for twice. A proven outage is not unproven by a
        // reply that came before it.
        (false, false, true, QueryEntry::CacheOnly),
        (false, true, true, QueryEntry::CacheOnly),
    ];

    for (ledgered, ever_confirmed, window_open, want) in table {
        assert_eq!(
            entry_of(ledgered, ever_confirmed, window_open),
            want,
            "ledgered={ledgered}, ever_confirmed={ever_confirmed}, window_open={window_open}"
        );
    }
}

/// What a cache read can serve: instances. An empty list is never served from
/// this call shape - `subscribe = true` reports a failed fetch as empty, so an
/// empty answer here cannot be read as "this name does not exist".
#[test]
fn a_cache_read_answers_only_with_instances() {
    let usable = vec![make_service_instance("10.0.0.1", 8080, true, true)];
    let answer = cache_hit_of("order", &usable).expect("a usable instance is answerable");
    match answer {
        CatalogAnswer::Instances(instances) => {
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].ip, IpAddr::from([10, 0, 0, 1]));
        }
        other => panic!("expected the instances to be served, got {other:?}"),
    }

    assert!(
        cache_hit_of("order", &[]).is_none(),
        "an empty list must fall through, not be answered as NXDOMAIN"
    );
    let unhealthy = vec![make_service_instance("10.0.0.2", 8080, false, true)];
    assert!(
        cache_hit_of("order", &unhealthy).is_none(),
        "a list with nothing usable says as little as an empty one"
    );
}

/// A cache hit is a map read and a clone, so it answers in microseconds. This
/// bound therefore only ever cuts off the round trip the SDK makes when the name
/// is not in the cache - and that call registers the listener, the refresh task
/// and the redo task before it waits, so cutting the wait off loses no
/// subscription.
#[test]
fn the_cache_read_bound_is_far_shorter_than_a_round_trip() {
    assert!(
        NACOS_CACHE_READ_TIMEOUT * 10 <= NACOS_LOOKUP_TIMEOUT,
        "a read taken instead of a call must not cost the call's budget: \
         {NACOS_CACHE_READ_TIMEOUT:?} against {NACOS_LOOKUP_TIMEOUT:?}"
    );
}
