#![allow(clippy::disallowed_methods)]

use async_trait::async_trait;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::server::{Request, RequestInfo};
use hickory_server::zone_handler::{
    AuthLookup, AxfrPolicy, LookupControlFlow, LookupOptions, LookupRecords, ZoneHandler, ZoneType,
};
use std::str::FromStr;
use std::sync::Arc;

/// A ZoneHandler that always returns Break(Ok(...)) with an empty record set.
struct BreakZoneHandler;

#[async_trait]
impl ZoneHandler for BreakZoneHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        static ORIGIN: std::sync::LazyLock<LowerName> =
            std::sync::LazyLock::new(|| LowerName::from(Name::from_str("break.local.").unwrap()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &LowerName,
        _rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Break(Ok(AuthLookup::answers(
            LookupRecords::Section(vec![]),
            None,
        )))
    }

    async fn search(
        &self,
        _request: &Request,
        _lookup_options: LookupOptions,
    ) -> (
        LookupControlFlow<AuthLookup>,
        Option<hickory_proto::rr::TSigResponseContext>,
    ) {
        (LookupControlFlow::Skip, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

/// A ZoneHandler that always returns Continue(Err(...)).
struct ErrorZoneHandler;

#[async_trait]
impl ZoneHandler for ErrorZoneHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        static ORIGIN: std::sync::LazyLock<LowerName> =
            std::sync::LazyLock::new(|| LowerName::from(Name::from_str("error.local.").unwrap()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &LowerName,
        _rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Continue(Err(
            hickory_server::zone_handler::LookupError::ResponseCode(
                hickory_proto::op::ResponseCode::ServFail,
            ),
        ))
    }

    async fn search(
        &self,
        _request: &Request,
        _lookup_options: LookupOptions,
    ) -> (
        LookupControlFlow<AuthLookup>,
        Option<hickory_proto::rr::TSigResponseContext>,
    ) {
        (LookupControlFlow::Skip, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

#[tokio::test]
async fn empty_chain_returns_skip() {
    let chain = AuthorityChain::new(vec![]);
    let name = Name::from_str("example.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Skip));
    assert!(result.handler_name.is_none());
}

#[tokio::test]
async fn single_handler_hit_returns_continue() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
    use std::sync::Arc;

    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "my-svc".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "10.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }],
    );

    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name = Name::from_str("my-svc.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Continue(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("nacos"));
}

#[tokio::test]
async fn single_handler_miss_is_authoritative_nxdomain() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use std::sync::Arc;

    let cache = Arc::new(DashMap::new());
    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name = Name::from_str("unknown.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(
        result.outcome,
        LookupControlFlow::Continue(Err(_))
    ));
    assert_eq!(result.handler_name.as_deref(), Some("nacos"));
}

#[tokio::test]
async fn first_skips_second_resolves() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
    use std::sync::Arc;

    // First handler has no data → will skip
    let empty_cache = Arc::new(DashMap::new());
    let first = NacosAuthority::new(empty_cache, "zone-a.local", 60);

    // Second handler has data → will resolve
    let populated_cache = Arc::new(DashMap::new());
    populated_cache.insert(
        ServiceKey {
            service_name: "web".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "192.168.1.1".to_string(),
            port: 80,
            healthy: true,
        }],
    );
    let second = NacosAuthority::new(populated_cache, "zone-b.local", 30);

    let chain = AuthorityChain::new(vec![
        ("zone-a".to_string(), Arc::new(first)),
        ("zone-b".to_string(), Arc::new(second)),
    ]);

    // Query matches zone-b, not zone-a → first skips, second resolves
    let name = Name::from_str("web.DEFAULT_GROUP.public.zone-b.local.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Continue(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("zone-b"));
}

#[tokio::test]
async fn all_handlers_skip_returns_skip() {
    use dashmap::DashMap;
    use doggy_dns_nacos::authority::NacosAuthority;
    use std::sync::Arc;

    let cache1 = Arc::new(DashMap::new());
    let cache2 = Arc::new(DashMap::new());
    let h1 = NacosAuthority::new(cache1, "zone-a.local", 60);
    let h2 = NacosAuthority::new(cache2, "zone-b.local", 60);

    let chain = AuthorityChain::new(vec![
        ("zone-a".to_string(), Arc::new(h1)),
        ("zone-b".to_string(), Arc::new(h2)),
    ]);

    let name = Name::from_str("svc.group.ns.zone-c.local.").unwrap();
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
    let chain = AuthorityChain::new(vec![("break".to_string(), Arc::new(BreakZoneHandler))]);
    assert!(!chain.is_empty());
}

#[tokio::test]
async fn break_result_stops_chain_and_returns_handler_name() {
    let chain = AuthorityChain::new(vec![
        ("breaker".to_string(), Arc::new(BreakZoneHandler)),
        ("should-not-reach".to_string(), Arc::new(ErrorZoneHandler)),
    ]);

    let name = Name::from_str("anything.example.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;

    // BreakZoneHandler returns Break(Ok(...)), so chain stops at first handler
    assert!(matches!(result.outcome, LookupControlFlow::Break(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("breaker"));
}

#[tokio::test]
async fn continue_error_propagated_with_handler_name() {
    let chain = AuthorityChain::new(vec![(
        "error-handler".to_string(),
        Arc::new(ErrorZoneHandler),
    )]);

    let name = Name::from_str("anything.example.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(
        result.outcome,
        LookupControlFlow::Continue(Err(_))
    ));
    assert_eq!(result.handler_name.as_deref(), Some("error-handler"));
}

#[tokio::test]
async fn authoritative_negative_carries_apex_soa() {
    use dashmap::DashMap;

    let authority = doggy_dns_nacos::authority::NacosAuthority::new(
        Arc::new(DashMap::new()),
        "nacos.local",
        60,
    );
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name = Name::from_str("missing.group.public.nacos.local.").unwrap();
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
async fn external_negative_is_not_authoritative_and_has_no_soa() {
    let chain = AuthorityChain::new(vec![(
        "external".to_string(),
        Arc::new(ExternalNegativeHandler),
    )]);

    let name = Name::from_str("missing.example.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(!result.authoritative);
    assert!(result.soa.is_none());
}

/// A non-authoritative handler that reports NODATA.
struct ExternalNegativeHandler;

#[async_trait]
impl ZoneHandler for ExternalNegativeHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::External
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        static ORIGIN: std::sync::LazyLock<LowerName> =
            std::sync::LazyLock::new(|| LowerName::from(Name::root()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &LowerName,
        _rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Continue(Err(hickory_server::zone_handler::LookupError::NameExists))
    }

    async fn search(
        &self,
        _request: &Request,
        _lookup_options: LookupOptions,
    ) -> (
        LookupControlFlow<AuthLookup>,
        Option<hickory_proto::rr::TSigResponseContext>,
    ) {
        (LookupControlFlow::Skip, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

use std::sync::atomic::{AtomicUsize, Ordering};

/// Authoritative handler that counts how many times the chain looked at it.
/// An in-zone negative answer used to cost two lookups: the answer and a
/// second one for the apex SOA.
struct CountingPrimaryHandler {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ZoneHandler for CountingPrimaryHandler {
    fn zone_type(&self) -> ZoneType {
        ZoneType::Primary
    }

    fn axfr_policy(&self) -> AxfrPolicy {
        AxfrPolicy::Deny
    }

    fn origin(&self) -> &LowerName {
        static ORIGIN: std::sync::LazyLock<LowerName> =
            std::sync::LazyLock::new(|| LowerName::from(Name::from_str("example.com.").unwrap()));
        &ORIGIN
    }

    async fn lookup(
        &self,
        _name: &LowerName,
        rtype: RecordType,
        _request_info: Option<&RequestInfo<'_>>,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if rtype == RecordType::SOA {
            let soa = hickory_proto::rr::Record::from_rdata(
                Name::from_str("example.com.").unwrap(),
                30,
                hickory_proto::rr::RData::SOA(hickory_proto::rr::rdata::SOA::new(
                    Name::from_str("example.com.").unwrap(),
                    Name::from_str("admin.example.com.").unwrap(),
                    1,
                    3600,
                    600,
                    86400,
                    30,
                )),
            );
            return LookupControlFlow::Continue(Ok(AuthLookup::answers(
                LookupRecords::Section(vec![soa]),
                None,
            )));
        }
        LookupControlFlow::Continue(Err(
            hickory_server::zone_handler::LookupError::ResponseCode(
                hickory_proto::op::ResponseCode::NXDomain,
            ),
        ))
    }

    async fn search(
        &self,
        _request: &Request,
        _lookup_options: LookupOptions,
    ) -> (
        LookupControlFlow<AuthLookup>,
        Option<hickory_proto::rr::TSigResponseContext>,
    ) {
        (LookupControlFlow::Skip, None)
    }

    async fn nsec_records(
        &self,
        _name: &LowerName,
        _lookup_options: LookupOptions,
    ) -> LookupControlFlow<AuthLookup> {
        LookupControlFlow::Skip
    }
}

#[tokio::test]
async fn negative_answers_fetch_the_apex_soa_only_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(CountingPrimaryHandler {
        calls: Arc::clone(&calls),
    });
    let chain = AuthorityChain::new(vec![("counting".to_string(), handler)]);

    let first = chain
        .resolve(
            &Name::from_str("missing.example.com.").unwrap(),
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
            &Name::from_str("other.example.com.").unwrap(),
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
