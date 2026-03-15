#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::watcher::{CachedInstance, InstanceChangeListener, ServiceKey};
use nacos_sdk::api::naming::{NamingChangeEvent, NamingEventListener, ServiceInstance};
use std::collections::HashMap;
use std::sync::Arc;

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

    let key = ServiceKey {
        service_name: "my-svc".to_string(),
        group: "default_group".to_string(),
        namespace: "public".to_string(),
    };
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

    let key = ServiceKey {
        service_name: "svc-a".to_string(),
        group: "default_group".to_string(),
        namespace: "public".to_string(),
    };
    let cached = cache.get(&key).unwrap();
    assert_eq!(cached.value().len(), 1);
    assert_eq!(cached.value()[0].ip, "10.0.0.1");
}

#[test]
fn listener_removes_key_when_instances_none() {
    let (listener, cache) = make_listener("default_group", "public");

    // Insert first
    let instances = vec![make_service_instance("10.0.0.1", 8080, true, true)];
    fire_event(&listener, "svc-remove", Some(instances));

    let key = ServiceKey {
        service_name: "svc-remove".to_string(),
        group: "default_group".to_string(),
        namespace: "public".to_string(),
    };
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

    let key = ServiceKey {
        service_name: "svc-replace".to_string(),
        group: "default_group".to_string(),
        namespace: "public".to_string(),
    };
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
    assert_eq!(cache.get(&key).unwrap().value()[0].ip, "10.0.0.2");
}

#[test]
fn cached_instance_from_service_instance() {
    let si = make_service_instance("192.168.1.100", 9090, true, true);
    let cached = CachedInstance::from(&si);
    assert_eq!(cached.ip, "192.168.1.100");
    assert_eq!(cached.port, 9090);
    assert!(cached.healthy);
}

#[test]
fn listener_lowercases_keys() {
    let (listener, cache) = make_listener("DEFAULT_GROUP", "Public");

    fire_event(
        &listener,
        "My-SVC",
        Some(vec![make_service_instance("10.0.0.1", 8080, true, true)]),
    );

    let key = ServiceKey {
        service_name: "my-svc".to_string(),
        group: "default_group".to_string(),
        namespace: "public".to_string(),
    };
    assert!(cache.get(&key).is_some());
}
