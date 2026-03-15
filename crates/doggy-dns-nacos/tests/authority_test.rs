#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use hickory_proto::rr::{LowerName, Name, RecordType};
use hickory_server::zone_handler::{LookupControlFlow, LookupOptions, ZoneHandler};
use std::str::FromStr;
use std::sync::Arc;

fn make_test_cache() -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    // Keys are stored lowercase since DNS names are case-insensitive
    cache.insert(
        ServiceKey {
            service_name: "user-service".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![
            CachedInstance {
                ip: "10.0.1.5".to_string(),
                port: 8080,
                healthy: true,
            },
            CachedInstance {
                ip: "10.0.1.6".to_string(),
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
async fn skips_unknown_service() {
    let cache = make_test_cache();
    let authority = NacosAuthority::new(cache, "nacos.local", 6);

    let name =
        LowerName::from(Name::from_str("unknown.default_group.public.nacos.local.").unwrap());

    let result = authority
        .lookup(&name, RecordType::A, None, LookupOptions::default())
        .await;

    assert!(matches!(result, LookupControlFlow::Skip));
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
