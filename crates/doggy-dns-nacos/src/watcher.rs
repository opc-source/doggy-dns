use crate::authority::ServiceCatalog;
use anyhow::anyhow;
use dashmap::DashSet;
use nacos_sdk::api::naming::{
    NamingChangeEvent, NamingEventListener, NamingService, NamingServiceBuilder, ServiceInstance,
};
use nacos_sdk::api::props::ClientProps;
use regex::Regex;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

/// Separator inside [`ServiceKey`].
///
/// Not a byte that can appear in a DNS label anyone will register, so a service
/// name containing dots can never be re-read as a group or namespace boundary.
pub const KEY_SEPARATOR: char = '\u{1}';

/// Key for a service lookup: `service SEPARATOR group SEPARATOR namespace`, all
/// lowercase.
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

    /// The three parts, borrowed. The only production caller is built from DNS
    /// labels, where `label_text` rejects the separator, so the split cannot land
    /// inside a service name — dotted names stay in one piece.
    pub fn parts(&self) -> (&str, &str, &str) {
        let mut parts = self.0.splitn(3, KEY_SEPARATOR);
        (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        )
    }
}

/// The built-in subscription rule: a bare application name — ASCII letters,
/// digits, `_`, `-`, and no dots.
///
/// Spelled as an explicit class rather than `\w`: with the regex crate's default
/// `unicode` feature `\w` also matches CJK, so writing the rule that way would
/// quietly widen the built-in default to names an operator did not ask for.
/// Configured patterns keep that same Unicode behaviour, deliberately.
const DEFAULT_SERVICE_PATTERN: &str = "[A-Za-z0-9_-]+";

/// Which Nacos services this process answers for.
///
/// A name this filter rejects is never sent to Nacos, and the authority owns its
/// whole zone, so querying it yields authoritative NXDOMAIN rather than a
/// fall-through to another plugin.
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

    /// True when `service_name`, exactly as Nacos reports it, should be served.
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
    /// Build a record source, rejecting an instance whose address is not an IP.
    ///
    /// Parsed once per query read instead of once per record, so an unparsable
    /// address is dropped before the family filter walks the list.
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

/// The instances worth answering with: healthy, enabled, and carrying an address
/// that parses. The client keeps instances this resolver cannot use; they are
/// dropped when a query reads the list.
pub fn usable_instances(instances: &[ServiceInstance]) -> Vec<CachedInstance> {
    instances
        .iter()
        .filter(|instance| instance.healthy && instance.enabled)
        .filter_map(CachedInstance::from_service_instance)
        .collect()
}

/// The listener a subscription carries.
///
/// No data flows through it: the SDK's emitter writes the client's naming cache
/// before it notifies anyone, and DNS reads that cache directly. A listener is
/// still required by `NamingService::subscribe`, and its presence keeps the SDK
/// from dropping the change notification with "the key ... is not subscribed".
struct NoopListener;

impl NamingEventListener for NoopListener {
    fn event(&self, _event: Arc<NamingChangeEvent>) {}
}

/// Answers from the Nacos client's own naming cache.
///
/// No instance data is kept here. Each query reads `get_all_instances` with
/// `subscribe = true`, which the SDK answers from its naming cache when it has
/// an entry and otherwise fetches; because every name is subscribed before it is
/// read, pushes keep that cache current — including a push that removes the last
/// instance, which reaches DNS on the next query.
pub struct NacosServiceWatcher {
    naming_service: Arc<NamingService>,
    namespace: String,
    group: String,
    filter: ServiceFilter,
    listener: Arc<dyn NamingEventListener>,
    /// Names whose subscription this process has registered.
    ///
    /// The SDK offers no way to ask whether a key is subscribed and keeps every
    /// listener it is handed, so this set is what makes the first query for a
    /// name the only one that subscribes it. It holds no instance data.
    subscribed: DashSet<String>,
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
            .app_name(doggy_dns_plugin::APP_NAME)
            // A service whose last instance disappears must reach the cache: with
            // the SDK's empty-push protection on, that push is dropped and the
            // stale address keeps being served until something else changes.
            .naming_push_empty_protection(false)
            // Load the client's naming cache from disk at start-up: a restart
            // answers from the previous run's data, and a Nacos outage at boot no
            // longer fails the build (the client retries in the background).
            .naming_load_cache_at_start(true);

        let naming_service = NamingServiceBuilder::new(props).build().await?;

        Ok(Self {
            naming_service: Arc::new(naming_service),
            namespace: namespace.to_string(),
            group: group.to_string(),
            filter,
            listener: Arc::new(NoopListener),
            subscribed: DashSet::new(),
        })
    }

    /// How many names this process has subscribed to, for the control plane's
    /// per-zone live signal.
    pub fn subscribed_count(&self) -> usize {
        self.subscribed.len()
    }
}

#[async_trait::async_trait]
impl ServiceCatalog for NacosServiceWatcher {
    /// The client cache's answer for `key`, subscribing the name first if this
    /// process never has.
    ///
    /// Bounded three ways: the key has to belong to the configured group and
    /// namespace, [`ServiceFilter`] has to select the name, and the subscribed
    /// set makes the first query for a name the only one that spends a
    /// `subscribe` call.
    async fn instances(&self, key: &ServiceKey) -> Option<Vec<CachedInstance>> {
        let (service, group, namespace) = key.parts();
        if !group.eq_ignore_ascii_case(&self.group)
            || !namespace.eq_ignore_ascii_case(&self.namespace)
        {
            // In our zone but not in the group we serve; nothing to ask.
            return None;
        }
        if !self.filter.is_selected(service) {
            tracing::debug!(
                "nacos service '{}' left unsubscribed: service_pattern did not select it",
                service
            );
            return None;
        }

        if self.subscribed.insert(service.to_string()) {
            let subscribed = self
                .naming_service
                .subscribe(
                    service.to_string(),
                    Some(self.group.clone()),
                    vec![],
                    Arc::clone(&self.listener),
                )
                .await;
            if let Err(e) = subscribed {
                // Forget the attempt so the next query retries it.
                self.subscribed.remove(service);
                tracing::warn!(
                    "failed to subscribe to nacos service '{}': {}; the next query retries it",
                    service,
                    e
                );
                return None;
            }
            tracing::info!("subscribed to nacos service: {}", service);
        }

        let instances = match self
            .naming_service
            .get_all_instances(
                service.to_string(),
                Some(self.group.clone()),
                vec![],
                // Reads the client's cache when it has an entry, which is what
                // keeps the common query local.
                true,
            )
            .await
        {
            Ok(instances) => instances,
            Err(e) => {
                tracing::warn!(
                    "failed to read the nacos client cache for service '{}': {}",
                    service,
                    e
                );
                return None;
            }
        };

        let usable = usable_instances(&instances);
        if usable.is_empty() {
            // Either Nacos does not know the name, or it has nothing healthy to
            // serve; the client cache carries no presence flag, so both answer
            // authoritative NXDOMAIN rather than NODATA.
            tracing::debug!("nacos has no usable instance of '{}'", service);
            return None;
        }
        Some(usable)
    }
}
