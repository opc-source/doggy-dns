use anyhow::anyhow;
use dashmap::DashMap;
use doggy_dns_basic::time::unix_now_secs;
use nacos_sdk::api::naming::{
    NamingChangeEvent, NamingEventListener, NamingService, NamingServiceBuilder, ServiceInstance,
};
use nacos_sdk::api::props::ClientProps;
use regex::Regex;
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

/// How many services one `get_service_list` page asks for while scanning the
/// whole list. Not configurable: the scan reads every page either way, so this
/// only trades round trips against the size of each answer.
const SERVICE_LIST_PAGE_SIZE: i32 = 1000;

/// How many pages of `page_size` it takes to cover `total` reported services.
/// `page_size` is positive, as every caller passes the constant above.
///
/// The first page carries the count, so the scan knows its length without
/// guessing; a count that is not positive, or a stale one the server retracts
/// mid-scan, costs at most one empty page because [`NacosServiceWatcher`] stops
/// on the first empty answer.
pub fn service_list_page_count(total: i32, page_size: i32) -> i32 {
    if total <= 0 {
        return 0;
    }
    // Ceiling divide written out: `i32::div_ceil` is still unstable.
    total / page_size + i32::from(total % page_size != 0)
}

/// The built-in subscription rule: a bare application name — ASCII letters,
/// digits, `_`, `-`, and no dots.
///
/// Spelled as an explicit class rather than `\w`: with the regex crate's default
/// `unicode` feature `\w` also matches CJK, so writing the rule that way would
/// quietly widen the built-in default to names an operator did not ask for.
/// Configured patterns keep that same Unicode behaviour, deliberately.
const DEFAULT_SERVICE_PATTERN: &str = "[A-Za-z0-9_-]+";

/// Which Nacos services this process subscribes to.
///
/// `get_service_list` reports every service in the group, and a shared
/// Nacos carries registrations this zone has no business being pushed, so the
/// list is narrowed before any subscription is sent. A name this filter rejects
/// never reaches the instance cache, and the authority owns its whole zone, so
/// querying it yields authoritative NXDOMAIN rather than a fall-through.
#[derive(Debug)]
pub struct ServiceFilter {
    pattern: Regex,
}

impl ServiceFilter {
    /// Compile the configured pattern, or [`DEFAULT_SERVICE_PATTERN`] when unset.
    ///
    /// The pattern is wrapped in whole-name anchors, so `order-service` selects
    /// that service and not `legacy-order-service`; a pattern that already spells
    /// its own anchors still compiles, and matching stays case-sensitive because
    /// the cache key is lowercased after this point, not here.
    pub fn from_config(pattern: Option<&str>) -> anyhow::Result<Self> {
        let configured = pattern.unwrap_or(DEFAULT_SERVICE_PATTERN);
        let compiled = Regex::new(&format!("^(?:{configured})$"))
            .map_err(|e| anyhow!("service_pattern '{configured}' is not a valid regex: {e}"))?;
        Ok(Self { pattern: compiled })
    }

    /// True when `service_name`, exactly as Nacos reports it, should be subscribed.
    pub fn is_selected(&self, service_name: &str) -> bool {
        self.pattern.is_match(service_name)
    }
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
    filter: ServiceFilter,
}

impl NacosServiceWatcher {
    pub async fn new(
        server_addr: &str,
        namespace: &str,
        group: &str,
        filter: ServiceFilter,
    ) -> anyhow::Result<Self> {
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
            filter,
        })
    }

    /// Subscribe to a specific service for push updates.
    pub async fn subscribe(&self, service_name: &str) -> anyhow::Result<()> {
        let listener = InstanceChangeListener {
            cache: Arc::clone(&self.cache),
            group: self.group.clone(),
            namespace: self.namespace.clone(),
            last_push: Arc::clone(&self.last_push),
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

    /// Subscribe to the services [`Self::filter`] selects from the whole service
    /// list of the configured group, read at start-up.
    pub async fn subscribe_all(&self) {
        let Some(service_names) = self.read_service_list().await else {
            return;
        };

        let (selected, filtered_out): (Vec<&String>, Vec<&String>) = service_names
            .iter()
            .partition(|service_name| self.filter.is_selected(service_name));

        for service_name in &filtered_out {
            tracing::debug!(
                "nacos service '{}' left unsubscribed: service_pattern did not select it",
                service_name
            );
        }
        for service_name in &selected {
            if let Err(e) = self.subscribe(service_name).await {
                tracing::warn!("failed to subscribe to service '{}': {}", service_name, e);
            }
        }
        tracing::info!(
            "subscribed to {} of {} nacos services in group {} ({} not selected by service_pattern)",
            selected.len(),
            service_names.len(),
            self.group,
            filtered_out.len()
        );
    }

    /// Read every page of the group's service list, sized from the count the
    /// first page reports. `None` only when that first request failed, which
    /// leaves nothing to subscribe.
    async fn read_service_list(&self) -> Option<Vec<String>> {
        let (mut service_names, pages) = match self
            .naming_service
            .get_service_list(1, SERVICE_LIST_PAGE_SIZE, Some(self.group.clone()))
            .await
        {
            Ok((names, total)) => (
                names,
                service_list_page_count(total, SERVICE_LIST_PAGE_SIZE),
            ),
            Err(e) => {
                tracing::warn!(
                    "failed to list the nacos services of group {}: {}; nothing is subscribed at start-up, so in-zone names answer NXDOMAIN until the list can be read",
                    self.group,
                    e
                );
                return None;
            }
        };

        for page_no in 2..=pages {
            match self
                .naming_service
                .get_service_list(page_no, SERVICE_LIST_PAGE_SIZE, Some(self.group.clone()))
                .await
            {
                Ok((page, _total)) => {
                    if page.is_empty() {
                        // The count went stale mid-scan; the server has no more.
                        break;
                    }
                    service_names.extend(page);
                }
                Err(e) => {
                    tracing::warn!(
                        "failed to read page {} of {} of the nacos service list of group {}: {}; subscribing to the {} names already read",
                        page_no,
                        pages,
                        self.group,
                        e,
                        service_names.len()
                    );
                    break;
                }
            }
        }

        Some(service_names)
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
