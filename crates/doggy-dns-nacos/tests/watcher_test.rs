#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::watcher::{
    CachedInstance, InstanceChangeListener, ServiceFilter, ServiceKey, service_list_page_count,
};
use nacos_sdk::api::naming::{NamingChangeEvent, NamingEventListener, ServiceInstance};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

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

fn make_listener(
    group: &str,
    namespace: &str,
) -> (
    InstanceChangeListener,
    Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
) {
    let cache = Arc::new(DashMap::new());
    let listener = InstanceChangeListener {
        cache: Arc::clone(&cache),
        group: group.to_string(),
        namespace: namespace.to_string(),
        last_push: Arc::new(AtomicI64::new(0)),
    };
    (listener, cache)
}

fn fire_event(
    listener: &InstanceChangeListener,
    service_name: &str,
    instances: Option<Vec<ServiceInstance>>,
) {
    let event = Arc::new(NamingChangeEvent {
        service_name: service_name.to_string(),
        group_name: String::new(),
        clusters: String::new(),
        instances,
    });
    listener.event(event);
}

#[test]
fn listener_inserts_healthy_enabled_instances() {
    let (listener, cache) = make_listener("default_group", "public");

    let instances = vec![
        make_service_instance("10.0.0.1", 8080, true, true),
        make_service_instance("10.0.0.2", 8080, true, true),
        make_service_instance("10.0.0.3", 8080, false, true), // unhealthy
    ];

    fire_event(&listener, "my-svc", Some(instances));

    let key = ServiceKey::new("my-svc", "default_group", "public");
    let cached = cache.get(&key).unwrap();
    assert_eq!(cached.value().len(), 2);
}

#[test]
fn listener_filters_disabled_instances() {
    let (listener, cache) = make_listener("default_group", "public");

    let instances = vec![
        make_service_instance("10.0.0.1", 8080, true, true),
        make_service_instance("10.0.0.2", 8080, true, false), // disabled
    ];

    fire_event(&listener, "svc-a", Some(instances));

    let key = ServiceKey::new("svc-a", "default_group", "public");
    let cached = cache.get(&key).unwrap();
    assert_eq!(cached.value().len(), 1);
    assert_eq!(cached.value()[0].ip, IpAddr::from([10, 0, 0, 1]));
}

#[test]
fn listener_removes_key_when_instances_none() {
    let (listener, cache) = make_listener("default_group", "public");

    // Insert first
    let instances = vec![make_service_instance("10.0.0.1", 8080, true, true)];
    fire_event(&listener, "svc-remove", Some(instances));

    let key = ServiceKey::new("svc-remove", "default_group", "public");
    assert!(cache.get(&key).is_some());

    // Fire with None → should remove
    fire_event(&listener, "svc-remove", None);
    assert!(cache.get(&key).is_none());
}

#[test]
fn listener_replaces_entire_vec_on_update() {
    let (listener, cache) = make_listener("default_group", "public");

    // First event: 1 instance
    fire_event(
        &listener,
        "svc-replace",
        Some(vec![make_service_instance("10.0.0.1", 8080, true, true)]),
    );

    let key = ServiceKey::new("svc-replace", "default_group", "public");
    assert_eq!(cache.get(&key).unwrap().value().len(), 1);

    // Second event: 3 instances (entire vec replaced)
    fire_event(
        &listener,
        "svc-replace",
        Some(vec![
            make_service_instance("10.0.0.2", 8080, true, true),
            make_service_instance("10.0.0.3", 8080, true, true),
            make_service_instance("10.0.0.4", 8080, true, true),
        ]),
    );
    assert_eq!(cache.get(&key).unwrap().value().len(), 3);
    assert_eq!(
        cache.get(&key).unwrap().value()[0].ip,
        IpAddr::from([10, 0, 0, 2])
    );
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

#[test]
fn listener_drops_only_the_bad_address() {
    let (listener, cache) = make_listener("default_group", "public");

    let mut broken = make_service_instance("garbage", 8080, true, true);
    broken.metadata = HashMap::new();
    fire_event(
        &listener,
        "svc-partial",
        Some(vec![
            make_service_instance("10.0.0.1", 8080, true, true),
            broken,
        ]),
    );

    let key = ServiceKey::new("svc-partial", "default_group", "public");
    let cached = cache.get(&key).expect("one valid instance is enough");
    assert_eq!(cached.value().len(), 1);
    assert_eq!(cached.value()[0].ip, IpAddr::from([10, 0, 0, 1]));
}

#[test]
fn listener_lowercases_keys() {
    let (listener, cache) = make_listener("DEFAULT_GROUP", "Public");

    fire_event(
        &listener,
        "My-SVC",
        Some(vec![make_service_instance("10.0.0.1", 8080, true, true)]),
    );

    let key = ServiceKey::new("my-svc", "default_group", "public");
    assert!(cache.get(&key).is_some());
}

#[test]
fn listener_stamps_last_push_on_every_event() {
    let (listener, _cache) = make_listener("default_group", "public");
    assert_eq!(
        listener.last_push.load(Ordering::Relaxed),
        0,
        "no event yet must read as epoch-0 (never pushed)"
    );

    fire_event(
        &listener,
        "svc-time",
        Some(vec![make_service_instance("10.0.0.1", 8080, true, true)]),
    );
    let first = listener.last_push.load(Ordering::Relaxed);
    assert!(first > 0, "a push event must stamp unix seconds");

    // Removal events are pushes too — freshness of contact matters, not payload.
    fire_event(&listener, "svc-time", None);
    assert!(listener.last_push.load(Ordering::Relaxed) >= first);
}

/// `service.group.namespace` is ambiguous with plain dots: `a.b` in group `c`
/// looks like `a` in group `b.c`. The separator is not a DNS label byte, so the
/// two stay distinct, which is what lets the composite key replace three fields.
#[test]
fn service_key_keeps_dotted_shapes_distinct() {
    let dotted_service = ServiceKey::new("a.b", "c", "d");
    let dotted_group = ServiceKey::new("a", "b.c", "d");

    assert_eq!(dotted_service.as_str(), "a.b\u{1}c\u{1}d");
    assert_ne!(
        dotted_service, dotted_group,
        "a service name containing dots must not be re-read as a group boundary"
    );
}

/// A Nacos identifier carrying the separator could forge a boundary and alias a
/// different triple, so the listener refuses to write such a key.
#[test]
fn listener_skips_updates_that_could_alias_another_key() {
    let (listener, cache) = make_listener("b\u{1}c", "public");

    fire_event(
        &listener,
        "a",
        Some(vec![make_service_instance("10.0.0.1", 8080, true, true)]),
    );

    assert!(
        cache.is_empty(),
        "a separator inside the group must not produce a composite key"
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

/// The first page reports how many services exist, and that count decides how
/// many more pages the scan asks for. Each row is the number of pages
/// `subscribe_all` fetches for a reported total at a 1000-entry page.
#[test]
fn page_count_covers_the_reported_total() {
    for (total, want) in [
        (0, 0),    // nothing registered: the first page answers empty
        (1, 1),    // a single service needs the first page, and only it
        (999, 1),  // one page still holds it
        (1000, 1), // exact fit must not ask for a second page
        (1001, 2), // one over the page boundary
        (2000, 2),
        (2001, 3),
        (-5, 0),               // a nonsense count fetches nothing beyond the first page
        (i32::MAX, 2_147_484), // ceil(2147483647 / 1000), without overflowing
    ] {
        assert_eq!(
            service_list_page_count(total, 1000),
            want,
            "total {total} must need {want} pages"
        );
    }
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
