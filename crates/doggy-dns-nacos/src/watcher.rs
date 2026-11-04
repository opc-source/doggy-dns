use dashmap::DashMap;
use doggy_dns_basic::time::unix_now_secs;
use nacos_sdk::api::naming::{
    NamingChangeEvent, NamingEventListener, NamingService, NamingServiceBuilder, ServiceInstance,
};
use nacos_sdk::api::props::ClientProps;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Separator inside [`ServiceKey`].
///
/// Not a byte that can appear in a DNS label anyone will register, so a service
/// name containing dots can never be re-read as a group or namespace boundary.
pub const KEY_SEPARATOR: char = '\u{1}';

/// Key for the service instance cache: `service SEPARATOR group SEPARATOR
/// namespace`, all lowercase.
///
/// One composite string instead of three fields: a lookup used to allocate the
/// three parts and then lowercase each of them, six allocations for a value the
/// caller already had in lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceKey(String);

impl ServiceKey {
    pub fn new(service_name: &str, group: &str, namespace: &str) -> Self {
        let mut key = String::with_capacity(service_name.len() + group.len() + namespace.len() + 2);
        key.push_str(service_name);
        key.push(KEY_SEPARATOR);
        key.push_str(group);
        key.push(KEY_SEPARATOR);
        key.push_str(namespace);
        Self(key)
    }

    /// The composite text. For logs and assertions; not used for lookup.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// True when no part contains the composite key separator, so the assembled key
/// cannot alias a different `(service, group, namespace)` triple.
fn parts_are_separable(parts: [&str; 3]) -> bool {
    parts.iter().all(|part| !part.contains(KEY_SEPARATOR))
}

/// Simplified service instance for DNS resolution.
#[derive(Debug, Clone)]
pub struct CachedInstance {
    pub ip: IpAddr,
    pub port: i32,
    pub healthy: bool,
}

impl CachedInstance {
    /// Build a cache entry, rejecting an instance whose address is not an IP.
    ///
    /// Parsed once per push instead of once per query. An unparsable address is
    /// dropped here rather than skipped while answering, so the warn log keeps
    /// its level and text but no longer repeats for every query.
    pub fn from_service_instance(instance: &ServiceInstance) -> Option<Self> {
        match IpAddr::from_str(&instance.ip) {
            Ok(ip) => Some(Self {
                ip,
                port: instance.port,
                healthy: instance.healthy,
            }),
            Err(e) => {
                tracing::warn!("skipping invalid IP '{}': {}", instance.ip, e);
                None
            }
        }
    }
}

/// Replace one service's cached instances, the whole update for that key.
///
/// One path for both writers: a push event and the read that follows a
/// subscription. Healthy and enabled filter here, an unparsable address is
/// dropped here, and the key is replaced rather than edited, so neither writer
/// can leave the other's half-update behind.
fn apply_instances(
    cache: &DashMap<ServiceKey, Vec<CachedInstance>>,
    group: &str,
    namespace: &str,
    service_name: &str,
    instances: Option<&[ServiceInstance]>,
) {
    if !parts_are_separable([service_name, group, namespace]) {
        tracing::warn!(
            service_name = %service_name,
            group = %group,
            namespace = %namespace,
            "nacos identifiers contain the cache key separator; skipping this update"
        );
        return;
    }

    let key = ServiceKey::new(
        &service_name.to_lowercase(),
        &group.to_lowercase(),
        &namespace.to_lowercase(),
    );

    match instances {
        Some(instances) => {
            let cached: Vec<CachedInstance> = instances
                .iter()
                .filter(|i| i.healthy && i.enabled)
                .filter_map(CachedInstance::from_service_instance)
                .collect();
            // Immutable replacement: insert replaces the entire value
            cache.insert(key, cached);
        }
        None => {
            cache.remove(&key);
        }
    }
}

pub struct InstanceChangeListener {
    pub cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    pub group: String,
    pub namespace: String,
    /// Unix seconds of the most recent push; 0 means "never received one".
    pub last_push: Arc<AtomicI64>,
}

impl NamingEventListener for InstanceChangeListener {
    fn event(&self, event: Arc<NamingChangeEvent>) {
        let service_name = event.service_name.clone();
        tracing::info!("nacos service changed: {}", service_name);
        self.last_push.store(unix_now_secs(), Ordering::Relaxed);

        apply_instances(
            &self.cache,
            &self.group,
            &self.namespace,
            &service_name,
            event.instances.as_deref(),
        );
    }
}

/// Watches Nacos for service instance changes and maintains an in-memory cache.
pub struct NacosServiceWatcher {
    naming_service: Arc<NamingService>,
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    namespace: String,
    group: String,
    last_push: Arc<AtomicI64>,
    /// Service name -> the exact [`Arc`] handed to the SDK at subscribe time.
    /// [`Self::unsubscribe_all`] must present that same `Arc`, because the SDK
    /// matches listeners by pointer, not by content.
    subscriptions: DashMap<String, Arc<InstanceChangeListener>>,
}

impl NacosServiceWatcher {
    pub async fn new(server_addr: &str, namespace: &str, group: &str) -> anyhow::Result<Self> {
        // Emergency start: `build()` health-checks the connection and would otherwise
        // fail a resolver whose Nacos is not up yet. The SDK re-sends each registered
        // subscription when the connection comes back, so the instance cache fills
        // itself once the server answers.
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name(doggy_dns_plugin::APP_NAME)
            .naming_load_cache_at_start(true);

        let naming_service = NamingServiceBuilder::new(props).build().await?;

        Ok(Self {
            naming_service: Arc::new(naming_service),
            cache: Arc::new(DashMap::new()),
            namespace: namespace.to_string(),
            group: group.to_string(),
            last_push: Arc::new(AtomicI64::new(0)),
            subscriptions: DashMap::new(),
        })
    }

    /// Subscribe to a specific service, then read its instances back.
    ///
    /// The read is not redundant: the SDK only notifies a listener about a
    /// service list that differs from its own cache, and with
    /// `naming_load_cache_at_start` that cache can be the disk snapshot this
    /// process loaded at boot. A service whose instances did not change would
    /// therefore never be pushed to us, so the subscription takes the list here
    /// instead of waiting for a change that may never come.
    pub async fn subscribe(&self, service_name: &str) -> anyhow::Result<()> {
        let listener = Arc::new(InstanceChangeListener {
            cache: Arc::clone(&self.cache),
            group: self.group.clone(),
            namespace: self.namespace.clone(),
            last_push: Arc::clone(&self.last_push),
        });

        self.naming_service
            .subscribe(
                service_name.to_string(),
                Some(self.group.clone()),
                vec![],
                Arc::clone(&listener) as Arc<dyn NamingEventListener>,
            )
            .await?;

        self.subscriptions
            .insert(service_name.to_string(), listener);

        match self
            .naming_service
            .get_all_instances(
                service_name.to_string(),
                Some(self.group.clone()),
                vec![],
                true,
            )
            .await
        {
            Ok(instances) => apply_instances(
                &self.cache,
                &self.group,
                &self.namespace,
                service_name,
                Some(&instances),
            ),
            Err(e) => {
                tracing::warn!(
                    "failed to read nacos instances for '{}': {}",
                    service_name,
                    e
                )
            }
        }

        tracing::info!("subscribed to nacos service: {}", service_name);
        Ok(())
    }

    /// Remove every subscription this watcher registered, best-effort.
    ///
    /// Used when a config reload retires a watcher whose plugin changed or
    /// went away. Unsubscribe failures are logged and skipped: teardown must
    /// never fail the reload that performs it. The SDK's connection itself
    /// lives until process exit (nacos-sdk 0.8.3 has no naming-client
    /// shutdown); what this removes are the server-side subscriptions, so a
    /// retired watcher stops being pushed instance updates.
    pub async fn unsubscribe_all(&self) {
        let entries: Vec<(String, Arc<dyn NamingEventListener>)> = self
            .subscriptions
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    Arc::clone(e.value()) as Arc<dyn NamingEventListener>,
                )
            })
            .collect();

        for (service_name, listener) in entries {
            if let Err(e) = self
                .naming_service
                .unsubscribe(
                    service_name.clone(),
                    Some(self.group.clone()),
                    vec![],
                    listener,
                )
                .await
            {
                tracing::warn!(
                    "failed to unsubscribe from service '{}': {}",
                    service_name,
                    e
                );
            }
        }
        self.subscriptions.clear();
        tracing::info!("unsubscribed from all nacos services");
    }

    /// Get cached instances for a service.
    pub fn get_instances(&self, key: &ServiceKey) -> Option<Vec<CachedInstance>> {
        self.cache.get(key).map(|entry| entry.value().clone())
    }

    /// Subscribe to all services in the configured namespace/group.
    /// Fetches the current service list and subscribes to each.
    pub async fn subscribe_all(&self) {
        match self
            .naming_service
            .get_service_list(1, 1000, Some(self.group.clone()))
            .await
        {
            Ok((service_names, _total)) => {
                for service_name in &service_names {
                    if let Err(e) = self.subscribe(service_name).await {
                        tracing::warn!("failed to subscribe to service '{}': {}", service_name, e);
                    }
                }
                tracing::info!("subscribed to {} nacos services", service_names.len());
            }
            Err(e) => {
                tracing::warn!(
                    "failed to list nacos services: {}, will rely on dynamic subscriptions",
                    e
                );
            }
        }
    }

    /// Get a reference to the cache.
    pub fn cache(&self) -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
        Arc::clone(&self.cache)
    }

    /// Unix seconds of the most recent push event shared with all listeners
    /// of this watcher; 0 means no push has ever arrived.
    pub fn last_push(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.last_push)
    }
}
