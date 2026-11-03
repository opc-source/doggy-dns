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

        if !parts_are_separable([&service_name, &self.group, &self.namespace]) {
            tracing::warn!(
                service_name = %service_name,
                group = %self.group,
                namespace = %self.namespace,
                "nacos identifiers contain the cache key separator; skipping this update"
            );
            return;
        }

        let key = ServiceKey::new(
            &service_name.to_lowercase(),
            &self.group.to_lowercase(),
            &self.namespace.to_lowercase(),
        );

        match &event.instances {
            Some(instances) => {
                let cached: Vec<CachedInstance> = instances
                    .iter()
                    .filter(|i| i.healthy && i.enabled)
                    .filter_map(CachedInstance::from_service_instance)
                    .collect();
                // Immutable replacement: insert replaces the entire value
                self.cache.insert(key, cached);
            }
            None => {
                self.cache.remove(&key);
            }
        }
    }
}

/// Watches Nacos for service instance changes and maintains an in-memory cache.
pub struct NacosServiceWatcher {
    naming_service: Arc<NamingService>,
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    namespace: String,
    group: String,
    last_push: Arc<AtomicI64>,
    /// Service name -> the exact `Arc` handed to the SDK at subscribe time.
    /// [`Self::unsubscribe_all`] has to present that same `Arc`, because the SDK
    /// matches a listener by pointer rather than by content, so a rebuilt
    /// equivalent would remove nothing.
    listeners: std::sync::Mutex<Vec<(String, Arc<InstanceChangeListener>)>>,
}

impl NacosServiceWatcher {
    pub async fn new(server_addr: &str, namespace: &str, group: &str) -> anyhow::Result<Self> {
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name(doggy_dns_plugin::APP_NAME);

        let naming_service = NamingServiceBuilder::new(props).build().await?;

        Ok(Self {
            naming_service: Arc::new(naming_service),
            cache: Arc::new(DashMap::new()),
            namespace: namespace.to_string(),
            group: group.to_string(),
            last_push: Arc::new(AtomicI64::new(0)),
            listeners: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Subscribe to a specific service for push updates.
    pub async fn subscribe(&self, service_name: &str) -> anyhow::Result<()> {
        let listener = Arc::new(InstanceChangeListener {
            cache: Arc::clone(&self.cache),
            group: self.group.clone(),
            namespace: self.namespace.clone(),
            last_push: Arc::clone(&self.last_push),
        });

        self.remember(service_name, Arc::clone(&listener));
        let registered = self
            .naming_service
            .subscribe(
                service_name.to_string(),
                Some(self.group.clone()),
                vec![],
                listener,
            )
            .await;
        match registered {
            Ok(()) => {
                tracing::info!("subscribed to nacos service: {}", service_name);
                Ok(())
            }
            Err(error) => {
                // Nothing was registered, so there is nothing to unsubscribe.
                self.forget(service_name);
                Err(error.into())
            }
        }
    }

    /// Register every subscription this watcher holds with the SDK, so a later
    /// retire can undo them.
    fn remember(&self, service_name: &str, listener: Arc<InstanceChangeListener>) {
        let mut listeners = self.lock_listeners();
        listeners.retain(|(name, _)| name != service_name);
        listeners.push((service_name.to_string(), listener));
    }

    /// Forget the subscription whose registration failed.
    fn forget(&self, service_name: &str) {
        let mut listeners = self.lock_listeners();
        listeners.retain(|(name, _)| name != service_name);
    }

    /// A poisoned lock means a subscribe panicked while holding it; the list is
    /// only bookkeeping for teardown, so its content is kept rather than lost.
    fn lock_listeners(
        &self,
    ) -> std::sync::MutexGuard<'_, Vec<(String, Arc<InstanceChangeListener>)>> {
        self.listeners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drop every push subscription this watcher registered.
    ///
    /// This does not close the naming connection — the SDK exposes no way to
    /// close one, which is why a watcher whose identity a reload no longer asks
    /// for is retired rather than freed. Unsubscribing is still what makes
    /// retirement mean something: without it a retired watcher keeps receiving
    /// pushes and filling a cache no authority reads.
    ///
    /// Failures are logged and skipped: this runs while a reload is being
    /// applied, and a server that will not acknowledge an unsubscribe is not a
    /// reason to abandon the reload.
    pub async fn unsubscribe_all(&self) {
        let pending = {
            let mut listeners = self.lock_listeners();
            std::mem::take(&mut *listeners)
        };
        let count = pending.len();
        for (service_name, listener) in pending {
            if let Err(error) = self
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
                    service = %service_name,
                    %error,
                    "failed to unsubscribe from a nacos service"
                );
            }
        }
        tracing::info!(services = count, "unsubscribed from nacos services");
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
