use crate::authority::{CatalogAnswer, ServiceCatalog};
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
use std::time::Duration;

/// The built-in subscription rule: a bare application name — ASCII letters,
/// digits, `_`, `-`, and no dots.
///
/// Spelled as an explicit class rather than `\w`: with the regex crate's default
/// `unicode` feature `\w` also matches CJK, so writing the rule that way would
/// quietly widen the built-in default to names an operator did not ask for.
/// Configured patterns keep that same Unicode behaviour, deliberately.
const DEFAULT_SERVICE_PATTERN: &str = "[A-Za-z0-9_-]+";

/// How long one query may wait on Nacos before it answers SERVFAIL.
///
/// The SDK retries a dead connection in the background and leaves a request
/// pending until the connection returns, which is longer than any DNS client
/// waits. The bound turns that wait into an answer the client can act on.
const NACOS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// its own anchors still compiles. Matching is case-sensitive, and the only
    /// production caller passes a name that `LowerName` already case-folded, so a
    /// pattern with capitals in it selects nothing.
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
///
/// One namespace, one group: the namespace is baked into the client's
/// configuration, and the group travels with every call, so a service name is
/// the only thing a query has to add.
pub struct NacosServiceWatcher {
    naming_service: Arc<NamingService>,
    group: String,
    filter: ServiceFilter,
    listener: Arc<dyn NamingEventListener>,
    /// Names whose subscription this process has registered.
    ///
    /// The SDK offers no way to ask whether a name is subscribed and keeps every
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

impl ServiceCatalog for NacosServiceWatcher {
    /// The client cache's answer for `service`, subscribing the name first if
    /// this process never has.
    ///
    /// Bounded three ways: [`ServiceFilter`] has to select the name, the
    /// subscribed set makes the first query for a name the only one that spends a
    /// `subscribe` call, and the wait on Nacos is capped at
    /// [`NACOS_LOOKUP_TIMEOUT`]. The group and namespace are not checked here:
    /// this client only ever asks the one pair it was built with, and the
    /// authority has already matched the name's labels against it.
    async fn instances(&self, service: &str) -> CatalogAnswer {
        if !self.filter.is_selected(service) {
            tracing::debug!(
                "nacos service '{}' left unsubscribed: service_pattern did not select it",
                service
            );
            return CatalogAnswer::Unknown;
        }

        match tokio::time::timeout(NACOS_LOOKUP_TIMEOUT, self.answer_from_nacos(service)).await {
            Ok(answer) => answer,
            Err(_) => {
                tracing::warn!(
                    "nacos did not answer for service '{}' within {:?}; answering SERVFAIL",
                    service,
                    NACOS_LOOKUP_TIMEOUT
                );
                CatalogAnswer::Unavailable
            }
        }
    }
}

impl NacosServiceWatcher {
    /// The part of [`ServiceCatalog::instances`] that may reach the network:
    /// subscribe the name once, then read the client cache.
    async fn answer_from_nacos(&self, service: &str) -> CatalogAnswer {
        // `contains` reads the `&str` this already has. Going straight to
        // `insert` would allocate a `String` on every query just to find out the
        // name is already subscribed - which, in steady state, it always is. If
        // two cold queries race through the gate, `insert` still returns `true`
        // for exactly one of them, so only one subscribe is sent.
        if !self.subscribed.contains(service) && self.subscribed.insert(service.to_string()) {
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
                return CatalogAnswer::Unavailable;
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
                return CatalogAnswer::Unavailable;
            }
        };

        let usable = usable_instances(&instances);
        if usable.is_empty() {
            // Either Nacos does not know the name, or it has nothing healthy to
            // serve; the client cache carries no presence flag, so both answer
            // authoritative NXDOMAIN rather than NODATA.
            tracing::debug!("nacos has no usable instance of '{}'", service);
            return CatalogAnswer::Unknown;
        }
        CatalogAnswer::Instances(usable)
    }
}
