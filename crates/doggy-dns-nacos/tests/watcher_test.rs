#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::watcher::{CachedInstance, InstanceChangeListener, ServiceKey};
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
