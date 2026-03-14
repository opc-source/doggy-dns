#![allow(clippy::disallowed_methods)]

use dns_filter_plugin::authority_chain::AuthorityChain;
use hickory_proto::rr::{Name, RecordType};
use hickory_server::zone_handler::LookupControlFlow;
use std::str::FromStr;

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
    use dns_filter_nacos::authority::NacosAuthority;
    use dns_filter_nacos::watcher::{CachedInstance, ServiceKey};
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
async fn single_handler_miss_returns_skip() {
    use dashmap::DashMap;
    use dns_filter_nacos::authority::NacosAuthority;
    use std::sync::Arc;

    let cache = Arc::new(DashMap::new());
    let authority = NacosAuthority::new(cache, "nacos.local", 60);
    let chain = AuthorityChain::new(vec![("nacos".to_string(), Arc::new(authority))]);

    let name = Name::from_str("unknown.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result.outcome, LookupControlFlow::Skip));
    assert!(result.handler_name.is_none());
}

#[tokio::test]
async fn first_skips_second_resolves() {
    use dashmap::DashMap;
    use dns_filter_nacos::authority::NacosAuthority;
    use dns_filter_nacos::watcher::{CachedInstance, ServiceKey};
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
    use dns_filter_nacos::authority::NacosAuthority;
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
