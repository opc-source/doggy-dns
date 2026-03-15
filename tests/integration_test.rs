#![allow(clippy::disallowed_methods)]

use dashmap::DashMap;
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{CachedInstance, ServiceKey};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use hickory_proto::rr::{Name, RecordType};
use hickory_server::zone_handler::LookupControlFlow;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

fn setup_nacos_cache() -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
    let cache = Arc::new(DashMap::new());
    cache.insert(
        ServiceKey {
            service_name: "web-app".to_string(),
            group: "default_group".to_string(),
            namespace: "public".to_string(),
        },
        vec![CachedInstance {
            ip: "10.0.0.1".to_string(),
            port: 8080,
            healthy: true,
        }],
    );
    cache
}

#[tokio::test]
async fn chain_nacos_hit() {
    let cache = setup_nacos_cache();
    let nacos = Arc::new(NacosAuthority::new(cache, "nacos.local", 6));
    let forward = Arc::new(
        ForwardAuthority::new(
            vec!["8.8.8.8:53".to_string()],
            256,
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await
        .unwrap(),
    );

    let chain = AuthorityChain::new(vec![
        ("nacos".to_string(), nacos),
        ("forward".to_string(), forward),
    ]);

    let name = Name::from_str("web-app.DEFAULT_GROUP.public.nacos.local.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(result.outcome, LookupControlFlow::Continue(Ok(_))));
    assert_eq!(result.handler_name.as_deref(), Some("nacos"));
}

#[tokio::test]
async fn chain_nacos_miss_falls_to_forward() {
    let cache = Arc::new(DashMap::new()); // empty cache
    let nacos = Arc::new(NacosAuthority::new(cache, "nacos.local", 6));
    let forward = Arc::new(
        ForwardAuthority::new(
            vec!["8.8.8.8:53".to_string()],
            256,
            Duration::from_secs(30),
            Duration::from_secs(600),
        )
        .await
        .unwrap(),
    );

    let chain = AuthorityChain::new(vec![
        ("nacos".to_string(), nacos),
        ("forward".to_string(), forward),
    ]);

    let name = Name::from_str("www.google.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;

    assert!(matches!(
        result.outcome,
        LookupControlFlow::Continue(Ok(_)) | LookupControlFlow::Break(Ok(_))
    ));
    assert_eq!(result.handler_name.as_deref(), Some("forward"));
}
