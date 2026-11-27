#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_testutil::MockZoneHandler;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::rdata::{A, SOA};
use hickory_proto::rr::{LowerName, Name, RData, Record, RecordType};
use hickory_server::zone_handler::{
    AuthLookup, LookupControlFlow, LookupError, LookupRecords, ZoneType,
};
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A handler that always breaks with an empty record set.
fn break_handler() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::Primary, "break.local.", |_, _, _, _| {
        LookupControlFlow::Break(Ok(AuthLookup::answers(
            LookupRecords::Section(vec![]),
            None,
        )))
    })
}

/// A handler that always fails the lookup with SERVFAIL.
fn error_handler() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::Primary, "error.local.", |_, _, _, _| {
        LookupControlFlow::Break(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
    })
}

fn external_servfail() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::External, ".", |_, _, _, _| {
        LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
    })
}

fn external_nxdomain() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::External, ".", |_, _, _, _| {
        LookupControlFlow::Break(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
    })
}

fn external_skip() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::External, ".", |_, _, _, _| {
        LookupControlFlow::Skip
    })
}

fn external_answer() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::External, ".", |name, _, _, _| {
        let record = Record::from_rdata(
            Name::from(name.clone()),
            60,
            RData::A(A(Ipv4Addr::new(203, 0, 113, 1))),
        );
        LookupControlFlow::Break(Ok(AuthLookup::answers(
            LookupRecords::Section(vec![record]),
            None,
        )))
    })
}

fn external_continue_answer() -> MockZoneHandler {
    MockZoneHandler::new(ZoneType::External, ".", |name, _, _, _| {
        let record = Record::from_rdata(
            Name::from(name.clone()),
            60,
            RData::A(A(Ipv4Addr::new(203, 0, 113, 1))),
        );
        LookupControlFlow::Continue(Ok(AuthLookup::answers(
            LookupRecords::Section(vec![record]),
            None,
        )))
    })
}

#[tokio::test]
async fn empty_chain_returns_skip() {
    let chain = AuthorityChain::new(vec![]);
    let name = LowerName::from(Name::from_str("example.com.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Skip));
    assert!(result.handler_name.is_none());
}

#[tokio::test]
async fn single_handler_hit_returns_break() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use doggy_dns_nacos::watcher::CachedInstance;
    use std::sync::Arc;

    let cache = Arc::new(DashMap::new());
    cache.insert(
        "my-svc".to_string(),
        vec![CachedInstance {
            ip: "10.0.0.1".parse().unwrap(),
            port: 8080,
            healthy: true,
        }],
    );

    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 60);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name = LowerName::from(Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Break(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("nacos"));
}

#[tokio::test]
async fn single_handler_miss_is_authoritative_nxdomain() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use std::sync::Arc;

    let cache = Arc::new(DashMap::new());
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 60);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name =
        LowerName::from(Name::from_str("unknown.DEFAULT_GROUP.public.nacos.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Break(Err(_))));
    assert_eq!(result.handler_name.as_deref(), Some("nacos"));
}

#[tokio::test]
async fn first_skips_second_resolves() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use doggy_dns_nacos::watcher::CachedInstance;
    use std::sync::Arc;

    // First handler has no data → will skip
    let empty_cache = Arc::new(DashMap::new());
    let first = NacosAuthority::new(empty_cache, "zone-a.local", "DEFAULT_GROUP", "public", 60);

    // Second handler has data → will resolve
    let populated_cache = Arc::new(DashMap::new());
    populated_cache.insert(
        "web".to_string(),
        vec![CachedInstance {
            ip: "192.168.1.1".parse().unwrap(),
            port: 80,
            healthy: true,
        }],
    );
    let second = NacosAuthority::new(
        populated_cache,
        "zone-b.local",
        "DEFAULT_GROUP",
        "public",
        30,
    );

    let chain = AuthorityChain::new(vec![
        ("zone-a".to_string(), Arc::new(first)),
        ("zone-b".to_string(), Arc::new(second)),
    ]);

    // Query matches zone-b, not zone-a → first skips, second resolves
    let name = LowerName::from(Name::from_str("web.DEFAULT_GROUP.public.zone-b.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Break(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("zone-b"));
}

#[tokio::test]
async fn all_handlers_skip_returns_skip() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use std::sync::Arc;

    let cache1 = Arc::new(DashMap::new());
    let cache2 = Arc::new(DashMap::new());
    let h1 = NacosAuthority::new(cache1, "zone-a.local", "DEFAULT_GROUP", "public", 60);
    let h2 = NacosAuthority::new(cache2, "zone-b.local", "DEFAULT_GROUP", "public", 60);

    let chain = AuthorityChain::new(vec![
        ("zone-a".to_string(), Arc::new(h1)),
        ("zone-b".to_string(), Arc::new(h2)),
    ]);

    let name = LowerName::from(Name::from_str("svc.group.ns.zone-c.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Skip));
    assert!(result.handler_name.is_none());
}

#[test]
fn is_empty_true_for_empty_chain() {
    let chain = AuthorityChain::new(vec![]);
    assert!(chain.is_empty());
}

#[test]
fn is_empty_false_for_non_empty_chain() {
    let chain = AuthorityChain::new(vec![("break".to_string(), Arc::new(break_handler()))]);
    assert!(!chain.is_empty());
}

#[tokio::test]
async fn break_result_stops_chain_and_returns_handler_name() {
    let chain = AuthorityChain::new(vec![
        ("breaker".to_string(), Arc::new(break_handler())),
        ("should-not-reach".to_string(), Arc::new(error_handler())),
    ]);

    let name = LowerName::from(Name::from_str("anything.example.com.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;

    // `break_handler` returns Break(Ok(...)), so the chain stops at the first handler
    assert!(matches!(result.outcome, LookupControlFlow::Break(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("breaker"));
}

#[tokio::test]
async fn break_error_propagated_with_handler_name() {
    let chain = AuthorityChain::new(vec![(
        "error-handler".to_string(),
        Arc::new(error_handler()),
    )]);

    let name = LowerName::from(Name::from_str("anything.example.com.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(result.outcome, LookupControlFlow::Break(Err(_))));
    assert_eq!(result.handler_name.as_deref(), Some("error-handler"));
}

#[tokio::test]
async fn authoritative_negative_carries_apex_soa() {
    use dashmap::DashMap;

    let authority = doggy_dns_nacos::authority::NacosAuthority::new(
        Arc::new(DashMap::new()),
        "nacos.local",
        "DEFAULT_GROUP",
        "public",
        60,
    );
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name =
        LowerName::from(Name::from_str("missing.default_group.public.nacos.local.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(
        result.authoritative,
        "Nacos zone must answer authoritatively"
    );
    let soa = result.soa.expect("negative answer must carry SOA");
    assert_eq!(soa.name, Name::from_str("nacos.local.").unwrap());
    assert_eq!(soa.ttl, 60);
}

#[tokio::test]
async fn external_servfail_falls_through_to_a_later_answer() {
    let chain = AuthorityChain::new(vec![
        ("native".to_string(), Arc::new(external_servfail())),
        ("forward".to_string(), Arc::new(external_answer())),
    ]);
    let name = LowerName::from(Name::from_str("example.com.").unwrap());

    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(result.outcome, LookupControlFlow::Break(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("forward"));
}

#[tokio::test]
async fn external_servfail_is_returned_when_nothing_else_answers() {
    let chain = AuthorityChain::new(vec![
        ("native".to_string(), Arc::new(external_servfail())),
        ("forward".to_string(), Arc::new(external_skip())),
    ]);
    let name = LowerName::from(Name::from_str("example.com.").unwrap());

    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(
        result.outcome,
        LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
    ));
    assert_eq!(result.handler_name.as_deref(), Some("native"));
}

#[tokio::test]
async fn first_continue_is_kept_when_later_handlers_continue_too() {
    let later = MockZoneHandler::new(ZoneType::External, ".", |_, _, _, _| {
        LookupControlFlow::Continue(Err(LookupError::NameExists))
    });
    let chain = AuthorityChain::new(vec![
        ("native".to_string(), Arc::new(external_servfail())),
        ("later".to_string(), Arc::new(later)),
    ]);
    let name = LowerName::from(Name::from_str("example.com.").unwrap());

    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(
        result.outcome,
        LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
    ));
    assert_eq!(result.handler_name.as_deref(), Some("native"));
}

#[tokio::test]
async fn continue_ok_is_returned_as_a_fallback() {
    let chain = AuthorityChain::new(vec![
        ("external".to_string(), Arc::new(external_continue_answer())),
        ("later".to_string(), Arc::new(external_skip())),
    ]);
    let name = LowerName::from(Name::from_str("example.com.").unwrap());

    let result = chain.resolve(&name, RecordType::A, None).await;

    match result.outcome {
        LookupControlFlow::Continue(Ok(lookup)) => {
            assert_eq!(lookup.iter().count(), 1, "the provisional answer survives");
        }
        other => panic!("expected Continue(Ok) fallback, got {other}"),
    }
    assert_eq!(result.handler_name.as_deref(), Some("external"));
}

#[tokio::test]
async fn external_nxdomain_does_not_fall_through() {
    let chain = AuthorityChain::new(vec![
        ("forward".to_string(), Arc::new(external_nxdomain())),
        ("native".to_string(), Arc::new(external_answer())),
    ]);
    let name = LowerName::from(Name::from_str("missing.example.com.").unwrap());

    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(
        result.outcome,
        LookupControlFlow::Break(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
    ));
    assert_eq!(result.handler_name.as_deref(), Some("forward"));
}

#[tokio::test]
async fn external_negative_is_not_authoritative_and_has_no_soa() {
    let chain = AuthorityChain::new(vec![(
        "external".to_string(),
        Arc::new(MockZoneHandler::new(
            ZoneType::External,
            ".",
            |_, _, _, _| LookupControlFlow::Break(Err(LookupError::NameExists)),
        )),
    )]);

    let name = LowerName::from(Name::from_str("missing.example.com.").unwrap());
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(!result.authoritative);
    assert!(result.soa.is_none());
}

#[tokio::test]
async fn negative_answers_fetch_the_apex_soa_only_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    // Authoritative handler that counts how many times the chain looked at it.
    // An in-zone negative answer used to cost two lookups: the answer and a
    // second one for the apex SOA.
    let counted = Arc::clone(&calls);
    let handler = MockZoneHandler::new(ZoneType::Primary, "example.com.", move |_, rtype, _, _| {
        counted.fetch_add(1, Ordering::Relaxed);
        if rtype == RecordType::SOA {
            let soa = Record::from_rdata(
                Name::from_str("example.com.").unwrap(),
                30,
                RData::SOA(SOA::new(
                    Name::from_str("example.com.").unwrap(),
                    Name::from_str("admin.example.com.").unwrap(),
                    1,
                    3600,
                    600,
                    86400,
                    30,
                )),
            );
            return LookupControlFlow::Break(Ok(AuthLookup::answers(
                LookupRecords::Section(vec![soa]),
                None,
            )));
        }
        LookupControlFlow::Break(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
    });
    let chain = AuthorityChain::new(vec![("counting".to_string(), Arc::new(handler))]);

    let first = chain
        .resolve(
            &LowerName::from(Name::from_str("missing.example.com.").unwrap()),
            RecordType::A,
            None,
        )
        .await;
    assert!(first.soa.is_some(), "authoritative negative carries an SOA");
    let after_first = calls.load(Ordering::Relaxed);
    assert_eq!(
        after_first, 2,
        "first negative costs one answer lookup plus one SOA lookup"
    );

    let second = chain
        .resolve(
            &LowerName::from(Name::from_str("other.example.com.").unwrap()),
            RecordType::A,
            None,
        )
        .await;
    assert!(
        second.soa.is_some(),
        "the cached SOA must still be attached"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed) - after_first,
        1,
        "second negative must cost only its own answer lookup, not two"
    );
}
