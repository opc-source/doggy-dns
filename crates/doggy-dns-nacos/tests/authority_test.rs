#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{LowerName, Name, RData, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupError, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::sync::Arc;

fn make_test_cache() -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    // Keys are stored lowercase since DNS names are case-insensitive
    cache.insert(
        ServiceKey::new("user-service", "default_group", "public"),
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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
        ServiceKey::new("com.example.api", "default_group", "public"),
        vec![CachedInstance {
            ip: "10.0.2.7".parse().unwrap(),
            port: 8080,
            healthy: true,
        }],
    );
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
        ServiceKey::new("user-service", "default_group", "public"),
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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
        ServiceKey::new("user-service", "default_group", "public"),
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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

    let name = LowerName::from(Name::from_str("www.google.com.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
}

#[tokio::test]
async fn skips_sibling_zone_with_shared_suffix() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
    let authority = NacosAuthority::new(cache, "nacos.local", 6);
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
    let authority = Arc::new(NacosAuthority::new(cache, "nacos.local", 6));
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
        published.is_some() && attached == published,
        "the chain must attach the same SOA the zone publishes"
    );
}

/// A client sends the QNAME in whatever case it likes. The cache key is
/// lowercase, and the only reason it matches is that the server hands every
/// handler a `LowerName` - so this is the contract the refactor relies on.
#[tokio::test]
async fn uppercase_wire_name_resolves_from_the_lowercase_cache() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

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
