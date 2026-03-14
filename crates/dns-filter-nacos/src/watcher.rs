use dashmap::DashMap;
use nacos_sdk::api::naming::{
    NamingChangeEvent, NamingEventListener, NamingService, NamingServiceBuilder, ServiceInstance,
};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;

/// Key for the service instance cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceKey {
    pub service_name: String,
    pub group: String,
    pub namespace: String,
}

/// Simplified service instance for DNS resolution.
#[derive(Debug, Clone)]
pub struct CachedInstance {
    pub ip: String,
    pub port: i32,
    pub healthy: bool,
}

impl From<&ServiceInstance> for CachedInstance {
    fn from(instance: &ServiceInstance) -> Self {
        Self {
            ip: instance.ip.clone(),
            port: instance.port,
            healthy: instance.healthy,
        }
    }
}

struct InstanceChangeListener {
    cache: Arc<DashMap<ServiceKey, Vec<CachedInstance>>>,
    group: String,
    namespace: String,
}

impl NamingEventListener for InstanceChangeListener {
    fn event(&self, event: Arc<NamingChangeEvent>) {
        let service_name = event.service_name.clone();
        tracing::info!("nacos service changed: {}", service_name);

        let key = ServiceKey {
            service_name: service_name.to_lowercase(),
            group: self.group.to_lowercase(),
            namespace: self.namespace.to_lowercase(),
        };

        match &event.instances {
            Some(instances) => {
                let cached: Vec<CachedInstance> = instances
                    .iter()
                    .filter(|i| i.healthy && i.enabled)
                    .map(CachedInstance::from)
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
}

impl NacosServiceWatcher {
    pub async fn new(
        server_addr: &str,
        namespace: &str,
        group: &str,
    ) -> anyhow::Result<Self> {
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name("dns-filter");

        let naming_service = NamingServiceBuilder::new(props).build().await?;

        Ok(Self {
            naming_service: Arc::new(naming_service),
            cache: Arc::new(DashMap::new()),
            namespace: namespace.to_string(),
            group: group.to_string(),
        })
    }

    /// Subscribe to a specific service for push updates.
    pub async fn subscribe(&self, service_name: &str) -> anyhow::Result<()> {
        let listener = InstanceChangeListener {
            cache: Arc::clone(&self.cache),
            group: self.group.clone(),
            namespace: self.namespace.clone(),
        };

        self.naming_service
            .subscribe(
                service_name.to_string(),
                Some(self.group.clone()),
                vec![],
                Arc::new(listener),
            )
            .await?;

        tracing::info!("subscribed to nacos service: {}", service_name);
        Ok(())
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
                        tracing::warn!(
                            "failed to subscribe to service '{}': {}",
                            service_name,
                            e
                        );
                    }
                }
                tracing::info!(
                    "subscribed to {} nacos services",
                    service_names.len()
                );
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
}
