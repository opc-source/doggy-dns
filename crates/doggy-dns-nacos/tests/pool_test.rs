#![allow(clippy::disallowed_methods)]

//! Reuse decisions a config reload makes about live Nacos connections.
//!
//! Both halves here are pure, which is the point: reconnecting is the expensive
//! and externally visible part of a reload, so the decision that avoids it is
//! worth testing without a Nacos server.

use doggy_dns_basic::PluginConfig;
use doggy_dns_nacos::pool::{NacosIdentity, NacosTarget, ReusePlan, plan_reuse};

fn nacos_plugin(
    server_addr: Option<&str>,
    namespace: Option<&str>,
    group: Option<&str>,
    dns_zone: Option<&str>,
) -> PluginConfig {
    PluginConfig {
        kind: doggy_dns_basic::PluginKind::Nacos,
        enabled: true,
        server_addr: server_addr.map(str::to_string),
        namespace: namespace.map(str::to_string),
        group: group.map(str::to_string),
        dns_zone: dns_zone.map(str::to_string),
        ttl: 6,
        cache_size: 4096,
        min_ttl: 10,
        max_ttl: 300,
        upstream: None,
    }
}

fn identity(server_addr: &str, namespace: &str, group: &str) -> NacosIdentity {
    NacosIdentity {
        server_addr: server_addr.to_string(),
        namespace: namespace.to_string(),
        group: group.to_string(),
    }
}

fn target(server_addr: &str, dns_zone: &str) -> NacosTarget {
    NacosTarget::from_plugin(&nacos_plugin(
        Some(server_addr),
        Some("public"),
        Some("DEFAULT_GROUP"),
        Some(dns_zone),
    ))
}

#[test]
fn unset_nacos_fields_fall_back_to_the_documented_defaults() {
    let resolved = NacosTarget::from_plugin(&nacos_plugin(None, None, None, None));

    assert_eq!(
        resolved.identity,
        identity("127.0.0.1:8848", "public", "DEFAULT_GROUP")
    );
    assert_eq!(resolved.zone, "nacos.local");
}

#[test]
fn configured_fields_are_used_verbatim() {
    // The pool keys on these strings, so a rewrite here (case folding, a
    // trailing dot stripped) would silently split one connection in two.
    let resolved = NacosTarget::from_plugin(&nacos_plugin(
        Some("nacos-headless:8848"),
        Some("Prod"),
        Some("MY_GROUP"),
        Some("Corp.local."),
    ));

    assert_eq!(
        resolved.identity,
        identity("nacos-headless:8848", "Prod", "MY_GROUP")
    );
    assert_eq!(resolved.zone, "Corp.local.");
    assert_eq!(resolved.ttl, 6);
}

#[test]
fn an_empty_pool_opens_every_target() {
    let targets = vec![target("a:8848", "a.local"), target("b:8848", "b.local")];
    assert_eq!(plan_reuse(&targets, &[]), vec![ReusePlan::Open; 2]);
}

#[test]
fn a_matching_identity_is_reused() {
    let targets = vec![target("a:8848", "a.local")];
    let pool = vec![identity("a:8848", "public", "DEFAULT_GROUP")];
    assert_eq!(plan_reuse(&targets, &pool), vec![ReusePlan::Existing(0)]);
}

#[test]
fn a_zone_change_reuses_the_connection() {
    // Zone and TTL shape the answers built on a cache, not the feed: republishing
    // a zone under a new name keeps the warm cache, which is the whole reason
    // identity excludes them.
    let targets = vec![target("a:8848", "renamed.local")];
    let pool = vec![identity("a:8848", "public", "DEFAULT_GROUP")];
    assert_eq!(plan_reuse(&targets, &pool), vec![ReusePlan::Existing(0)]);
}

#[test]
fn any_identity_component_difference_forces_a_new_connection() {
    for changed in [
        identity("b:8848", "public", "DEFAULT_GROUP"),
        identity("a:8848", "staging", "DEFAULT_GROUP"),
        identity("a:8848", "public", "OTHER_GROUP"),
    ] {
        let pool = vec![changed];
        assert_eq!(
            plan_reuse(&[target("a:8848", "a.local")], &pool),
            vec![ReusePlan::Open],
            "a pooled {pool:?} is not the connection the target asks for"
        );
    }
}

#[test]
fn matching_consumes_so_two_targets_on_one_server_take_two_watchers() {
    // Two nacos plugins on the same server with different zones get one watcher
    // each, exactly as they do at start-up: a single subscriber could not
    // publish two zones' worth of services from one cache.
    let targets = vec![target("a:8848", "x.local"), target("a:8848", "y.local")];
    let pool = vec![
        identity("a:8848", "public", "DEFAULT_GROUP"),
        identity("a:8848", "public", "DEFAULT_GROUP"),
    ];
    assert_eq!(
        plan_reuse(&targets, &pool),
        vec![ReusePlan::Existing(0), ReusePlan::Existing(1)]
    );

    // With only one watcher pooled, the second target opens its own rather than
    // double-booking the first.
    let pool = vec![identity("a:8848", "public", "DEFAULT_GROUP")];
    assert_eq!(
        plan_reuse(&targets, &pool),
        vec![ReusePlan::Existing(0), ReusePlan::Open]
    );
}

#[test]
fn plans_address_the_pool_as_it_was_on_entry() {
    // The reload loop appends to the pool while it works, so an index has to
    // stay valid for the whole build: indices refer to the snapshot, not to the
    // pool after any opening.
    let targets = vec![target("b:8848", "b.local"), target("a:8848", "a.local")];
    let pool = vec![
        identity("a:8848", "public", "DEFAULT_GROUP"),
        identity("c:8848", "public", "DEFAULT_GROUP"),
    ];
    assert_eq!(
        plan_reuse(&targets, &pool),
        vec![ReusePlan::Open, ReusePlan::Existing(0)]
    );
}

#[test]
fn a_watcher_no_target_asks_for_is_left_in_place() {
    // One plan per target, always: an unasked-for watcher cannot be closed (the
    // SDK exposes no shutdown), so it simply stays pooled until a restart.
    let targets = vec![target("a:8848", "a.local")];
    let pool = vec![
        identity("a:8848", "public", "DEFAULT_GROUP"),
        identity("gone:8848", "public", "DEFAULT_GROUP"),
    ];
    assert_eq!(plan_reuse(&targets, &pool), vec![ReusePlan::Existing(0)]);
}
