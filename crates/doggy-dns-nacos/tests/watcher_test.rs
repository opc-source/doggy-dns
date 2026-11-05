#![allow(clippy::disallowed_methods)]

use doggy_dns_nacos::watcher::{CachedInstance, ServiceFilter, usable_instances};
use nacos_sdk::api::naming::ServiceInstance;
use std::collections::HashMap;
use std::net::IpAddr;

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

/// Matching runs on the name as Nacos reports it; the cache key is lowercased
/// later, so nothing lowercases the pattern for the operator.
#[test]
fn filter_matching_is_case_sensitive() {
    let filter = ServiceFilter::from_config(Some("order")).expect("valid pattern");

    assert!(filter.is_selected("order"));
    assert!(
        !filter.is_selected("ORDER"),
        "a lowercase pattern must not silently select an uppercase registration"
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
