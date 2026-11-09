#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::authority::{CatalogAnswer, NacosAuthority, ServiceCatalog};
use doggy_dns_nacos::test_support::{instances, qname};
use doggy_dns_nacos::watcher::CachedInstance;
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{LowerName, Name, RData, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupError, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

fn make_test_cache() -> Arc<DashMap<String, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    // Keys are stored lowercase since DNS names are case-insensitive
    cache.insert(
        "user-service".to_string(),
        vec![
            CachedInstance {
                ip: "10.0.1.5".parse().unwrap(),
                port: 8080,
                healthy: true,
            },
            CachedInstance {
                ip: "10.0.1.6".parse().unwrap(),
                port: 8080,
                healthy: true,
            },
        ],
    );
    cache
}

#[tokio::test]
async fn resolves_known_service() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            let records: Vec<_> = lookup.iter().collect();
            assert_eq!(records.len(), 2);
        }
        _ => panic!("Expected Continue(Ok), got unexpected result"),
    }
}

#[tokio::test]
async fn resolves_dotted_service_name() {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "com.example.api".to_string(),
        vec![CachedInstance {
            ip: "10.0.2.7".parse().unwrap(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name = LowerName::from(
        Name::from_str("com.example.api.DEFAULT_GROUP.public.nacos.local.").unwrap(),
    );
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            assert_eq!(lookup.iter().count(), 1);
        }
        _ => panic!("Expected Continue(Ok), got unexpected result"),
    }
}

#[tokio::test]
async fn unknown_service_in_zone_returns_authoritative_nxdomain() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("unknown.default_group.public.nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
        ),
        "unknown service inside the zone must be authoritative NXDOMAIN"
    );
}

#[tokio::test]
async fn malformed_name_in_zone_returns_nxdomain() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name = LowerName::from(Name::from_str("user-service.nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(
        result,
        LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
    ));
}

#[tokio::test]
async fn zone_apex_returns_nodata() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name = LowerName::from(Name::from_str("nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(
        result,
        LookupControlFlow::Continue(Err(LookupError::NameExists))
    ));
}

#[tokio::test]
async fn zone_apex_soa_query_returns_soa_record() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name = LowerName::from(Name::from_str("nacos.local.").unwrap());
    let result = authority
        .lookup(&name, RecordType::SOA, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            let records: Vec<_> = lookup.iter().collect();
            assert_eq!(records.len(), 1);
            let record = records[0];
            assert_eq!(&record.name, &Name::from_str("nacos.local.").unwrap());
            assert_eq!(record.ttl, 6);
            let RData::SOA(soa) = &record.data else {
                panic!("expected SOA rdata, got {:?}", record.data);
            };
            assert_eq!(soa.minimum, 6);
            assert_eq!(&soa.mname, &Name::from_str("nacos.local.").unwrap());
        }
        _ => panic!("Expected Continue(Ok) with an SOA record"),
    }
}

#[tokio::test]
async fn family_without_records_returns_nodata() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::AAAA, None, LookupOptions::default())
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::NameExists))
        ),
        "A-only service queried for AAAA must be NODATA, not NXDOMAIN"
    );
}

#[tokio::test]
async fn a_query_returns_only_a_records() {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "user-service".to_string(),
        vec![
            CachedInstance {
                ip: "10.0.1.5".parse().unwrap(),
                port: 8080,
                healthy: true,
            },
            CachedInstance {
                ip: "2001:db8::1".parse().unwrap(),
                port: 8080,
                healthy: true,
            },
        ],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            let records: Vec<_> = lookup.iter().collect();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].record_type(), RecordType::A);
        }
        _ => panic!("Expected Continue(Ok), got unexpected result"),
    }
}

#[tokio::test]
async fn aaaa_query_returns_only_aaaa_records() {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        "user-service".to_string(),
        vec![
            CachedInstance {
                ip: "10.0.1.5".parse().unwrap(),
                port: 8080,
                healthy: true,
            },
            CachedInstance {
                ip: "2001:db8::1".parse().unwrap(),
                port: 8080,
                healthy: true,
            },
        ],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap());
    let result = authority
        .lookup(&name, RecordType::AAAA, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            let records: Vec<_> = lookup.iter().collect();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].record_type(), RecordType::AAAA);
        }
        _ => panic!("Expected Continue(Ok), got unexpected result"),
    }
}

#[tokio::test]
async fn unsupported_query_type_on_known_service_returns_nodata() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("user-service.DEFAULT_GROUP.public.nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::MX, None, LookupOptions::default())
        .await;

    assert!(matches!(
        result,
        LookupControlFlow::Continue(Err(LookupError::NameExists))
    ));
}

#[tokio::test]
async fn skips_wrong_zone() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
}

#[tokio::test]
async fn skips_sibling_zone_with_shared_suffix() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name = LowerName::from(
        Name::from_str("user-service.default_group.public.evilnacos.local.").unwrap(),
    );

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
}

/// The apex SOA is built once. Rebuilding it per answer was the cost; equal
/// rdata across calls is what a caller can observe, so assert that plus the
/// fact that a negative answer and an apex SOA query publish the same record.
#[tokio::test]
async fn apex_soa_is_identical_across_lookups() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);
    let apex = LowerName::from(Name::from_str("nacos.local.").unwrap());

    let mut soas = Vec::new();
    for _ in 0..3 {
        match authority
            .lookup(&apex, RecordType::SOA, None, LookupOptions::default())
            .await
        {
            LookupControlFlow::Continue(Ok(lookup)) => {
                soas.push(
                    lookup
                        .iter()
                        .next()
                        .expect("apex SOA query returns one record")
                        .clone(),
                );
            }
            other => panic!("expected Continue(Ok) for an apex SOA query, got {other}"),
        }
    }

    assert!(
        soas.windows(2).all(|pair| pair[0].data == pair[1].data
            && pair[0].name == pair[1].name
            && pair[0].ttl == pair[1].ttl),
        "repeated apex SOA lookups must return the same record"
    );
}

#[tokio::test]
async fn apex_soa_matches_the_record_a_negative_answer_carries() {
    let cache = make_test_cache();
    let authority = Arc::new(NacosAuthority::new(
        cache,
        "nacos.local",
        "DEFAULT_GROUP",
        "public",
        6,
    ));
    let apex = LowerName::from(Name::from_str("nacos.local.").unwrap());
    let missing =
        LowerName::from(Name::from_str("unknown.default_group.public.nacos.local.").unwrap());

    let published = match authority
        .lookup(&apex, RecordType::SOA, None, LookupOptions::default())
        .await
    {
        LookupControlFlow::Continue(Ok(lookup)) => lookup.iter().next().cloned(),
        other => panic!("expected an apex SOA, got {other}"),
    };

    let handler: Arc<dyn ZoneHandler> = authority.clone();
    let chain = AuthorityChain::new(vec![("nacos".to_string(), handler)]);
    let attached = chain.resolve(&missing, RecordType::A, None).await.soa;

    assert!(
        published.is_some() && attached.as_deref() == published.as_ref(),
        "the chain must attach the same SOA the zone publishes"
    );
}

/// Stands in for the Nacos client cache. Every in-zone query goes through it,
/// and it records the names so the test can prove when the catalog is and is not
/// consulted.
struct FakeCatalog {
    answer: CatalogAnswer,
    asked: Arc<Mutex<Vec<String>>>,
}

impl FakeCatalog {
    fn new(answer: CatalogAnswer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().expect("no poisoned mutex").clone()
    }
}

impl ServiceCatalog for FakeCatalog {
    async fn instances(&self, service: &str) -> CatalogAnswer {
        self.asked
            .lock()
            .expect("no poisoned mutex")
            .push(service.to_string());
        self.answer.clone()
    }
}

fn catalog_authority(
    answer: Option<Vec<CachedInstance>>,
) -> (NacosAuthority<FakeCatalog>, Arc<FakeCatalog>) {
    let answer = match answer {
        Some(instances) => CatalogAnswer::Instances(instances),
        None => CatalogAnswer::Unknown,
    };
    let catalog = FakeCatalog::new(answer);
    let authority =
        NacosAuthority::new(catalog.clone(), "nacos.local", "DEFAULT_GROUP", "public", 6);
    (authority, catalog)
}

fn unavailable_catalog_authority() -> (NacosAuthority<FakeCatalog>, Arc<FakeCatalog>) {
    let catalog = FakeCatalog::new(CatalogAnswer::Unavailable);
    let authority =
        NacosAuthority::new(catalog.clone(), "nacos.local", "DEFAULT_GROUP", "public", 6);
    (authority, catalog)
}

/// The authority keeps no copy of the instance list: both queries for the same
/// name read the catalog, so a deletion the client has already absorbed cannot
/// be answered from a stale second copy.
#[tokio::test]
async fn every_in_zone_address_query_is_answered_from_the_catalog() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.9.9.9"])));

    for _ in 0..2 {
        let result = authority
            .lookup(
                &qname("late-service.default_group.public.nacos.local."),
                RecordType::A,
                None,
                LookupOptions::default(),
            )
            .await;

        match result {
            LookupControlFlow::Continue(Ok(lookup)) => {
                let records: Vec<_> = lookup.iter().collect();
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].data.to_string(), "10.9.9.9");
            }
            other => panic!("expected the catalog instance to answer, got {other}"),
        }
    }

    assert_eq!(
        catalog.asked(),
        vec!["late-service".to_string(), "late-service".to_string()],
        "every query must read the catalog, with the service name the labels map to"
    );
}

/// A name in the zone but outside the group and namespace this plugin serves is
/// not this zone's to answer. The check has to live where the configured values
/// are known, so it belongs to the authority rather than the catalog behind it.
#[tokio::test]
async fn a_name_outside_the_configured_group_is_nxdomain_without_reading_the_catalog() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.8.8.8"])));

    let result = authority
        .lookup(
            &qname("late-service.other_group.public.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
        ),
        "a name in another group must stay authoritative NXDOMAIN, got {result}"
    );
    assert!(
        catalog.asked().is_empty(),
        "a group this zone does not serve must not reach Nacos: {:?}",
        catalog.asked()
    );
}

#[tokio::test]
async fn a_name_outside_the_configured_namespace_is_nxdomain_without_reading_the_catalog() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.8.8.8"])));

    let result = authority
        .lookup(
            &qname("late-service.default_group.other-ns.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
        ),
        "a name in another namespace must stay authoritative NXDOMAIN, got {result}"
    );
    assert!(
        catalog.asked().is_empty(),
        "a namespace this zone does not serve must not reach Nacos: {:?}",
        catalog.asked()
    );
}

/// The configured spelling keeps its case in `[[plugins]]` (`DEFAULT_GROUP`)
/// while DNS labels arrive lowercased, so the comparison is case-insensitive and
/// a legitimate query must still resolve.
#[tokio::test]
async fn the_configured_group_matches_a_case_folded_label() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.7.7.7"])));

    let result = authority
        .lookup(
            &qname("late-service.default_group.public.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            assert_eq!(lookup.iter().count(), 1, "the instance must answer");
        }
        other => panic!("expected the configured group to resolve, got {other}"),
    }
    assert_eq!(catalog.asked().len(), 1);
}

#[tokio::test]
async fn a_catalog_without_the_service_answers_nxdomain() {
    let (authority, catalog) = catalog_authority(None);

    let result = authority
        .lookup(
            &qname("never-registered.default_group.public.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
        ),
        "a name the catalog does not know must stay authoritative NXDOMAIN, got {result}"
    );
    assert_eq!(catalog.asked().len(), 1);
}

/// A backend failure is not a negative answer: SERVFAIL tells the client to try
/// again, where NXDOMAIN would be cached as "this name does not exist" for the
/// whole TTL and outlive a short Nacos outage.
#[tokio::test]
async fn a_catalog_backend_failure_answers_servfail() {
    let (authority, catalog) = unavailable_catalog_authority();

    let result = authority
        .lookup(
            &qname("during-outage.default_group.public.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::ServFail)))
        ),
        "a catalog that cannot answer must not turn an outage into NXDOMAIN, got {result}"
    );
    assert_eq!(catalog.asked().len(), 1);
}

/// A service the client still knows but which has no usable address is NODATA:
/// the name exists, it just has no record of the requested kind.
#[tokio::test]
async fn a_catalog_answer_with_no_usable_instance_is_nodata() {
    let (authority, _catalog) = catalog_authority(Some(Vec::new()));

    let result = authority
        .lookup(
            &qname("draining.default_group.public.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(
        matches!(
            result,
            LookupControlFlow::Continue(Err(LookupError::NameExists))
        ),
        "an existing service with no usable instance is NODATA, got {result}"
    );
}

/// Non-address queries also read the catalog: without a local copy the authority
/// cannot tell an existing service from an unknown one any other way, and the
/// two must answer NODATA and NXDOMAIN respectively.
#[tokio::test]
async fn non_address_queries_consult_the_catalog_and_answer_nodata() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.9.9.9"])));

    let result = authority
        .lookup(
            &qname("late-service.default_group.public.nacos.local."),
            RecordType::MX,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(matches!(
        result,
        LookupControlFlow::Continue(Err(LookupError::NameExists))
    ));
    assert_eq!(catalog.asked().len(), 1);
}

#[tokio::test]
async fn non_address_queries_without_a_catalog_answer_are_nxdomain() {
    for rtype in [RecordType::MX, RecordType::SRV, RecordType::TXT] {
        let (authority, catalog) = catalog_authority(None);

        let result = authority
            .lookup(
                &qname("never-registered.default_group.public.nacos.local."),
                rtype,
                None,
                LookupOptions::default(),
            )
            .await;

        assert!(
            matches!(
                result,
                LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
            ),
            "{rtype} on an unknown name stays NXDOMAIN, got {result}"
        );
        assert_eq!(catalog.asked().len(), 1);
    }
}

#[tokio::test]
async fn names_outside_the_zone_never_consult_the_catalog() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.9.9.9"])));

    let result = authority
        .lookup(
            &qname("www.google.com."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
    assert!(
        catalog.asked().is_empty(),
        "another zone's names must not be sent to Nacos, got {:?}",
        catalog.asked()
    );
}

#[tokio::test]
async fn the_zone_apex_never_consults_the_catalog() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.9.9.9"])));

    let result = authority
        .lookup(
            &qname("nacos.local."),
            RecordType::SOA,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(matches!(result, LookupControlFlow::Continue(Ok(_))));
    assert!(catalog.asked().is_empty(), "{:?}", catalog.asked());
}

#[tokio::test]
async fn a_name_without_group_and_namespace_labels_never_consults_the_catalog() {
    let (authority, catalog) = catalog_authority(Some(instances(&["10.9.9.9"])));

    let result = authority
        .lookup(
            &qname("user-service.nacos.local."),
            RecordType::A,
            None,
            LookupOptions::default(),
        )
        .await;

    assert!(matches!(
        result,
        LookupControlFlow::Continue(Err(LookupError::ResponseCode(ResponseCode::NXDomain)))
    ));
    assert!(catalog.asked().is_empty(), "{:?}", catalog.asked());
}

/// A client sends the QNAME in whatever case it likes. The cache key is
/// lowercase, and the only reason it matches is that the server hands every
/// handler a `LowerName` - so this is the contract the refactor relies on.
#[tokio::test]
async fn uppercase_wire_name_resolves_from_the_lowercase_cache() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", "DEFAULT_GROUP", "public", 6);

    let name =
        LowerName::from(Name::from_str("USER-SERVICE.DEFAULT_GROUP.PUBLIC.NACOS.LOCAL.").unwrap());
    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    match result {
        LookupControlFlow::Continue(Ok(lookup)) => {
            assert_eq!(lookup.iter().count(), 2, "both seeded instances answer");
        }
        other => panic!("uppercase QNAME must still resolve, got {other}"),
    }
}
