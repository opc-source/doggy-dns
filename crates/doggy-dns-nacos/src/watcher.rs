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
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
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
pub const NACOS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How many consecutive remote failures open the backoff window.
///
/// One failure is a query, two are a bad minute; the third is an outage worth
/// stopping for. A lower threshold would let a flaky link switch the remote off
/// for names no client has asked about yet.
pub const NACOS_FAILURE_THRESHOLD: u32 = 3;

/// How long the backoff window holds once it opens.
///
/// Long enough to ride out a Nacos restart without spending a timeout per query,
/// short enough that the registry is asked again soon after it recovers.
pub const NACOS_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

/// `opened_at` value meaning "no window".
const WINDOW_CLOSED: i64 = i64::MIN;

/// Counts consecutive remote failures and holds a backoff window once the
/// streak reaches [`NACOS_FAILURE_THRESHOLD`].
///
/// Only calls that actually reach Nacos belong here: a read answered from the
/// client's cache proves nothing about the server, so counting it would keep a
/// dead registry looking alive. The window is the watcher's signal to stop
/// spending round trips on names it has never confirmed.
///
/// The clock is a parameter of every call, never read from the process, so the
/// window's boundaries are testable without sleeping through them.
#[derive(Debug)]
pub struct FailureBackoff {
    failures: AtomicU32,
    opened_at: AtomicI64,
}

impl Default for FailureBackoff {
    fn default() -> Self {
        Self {
            failures: AtomicU32::new(0),
            opened_at: AtomicI64::new(WINDOW_CLOSED),
        }
    }
}

impl FailureBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count a remote failure at `now`; true when the window is open as a
    /// result.
    ///
    /// A failure past the threshold re-stamps the window and also returns true,
    /// so one failure after an expired window reopens it: the outage is already
    /// proven, and making the remote pay the whole threshold again would spend
    /// another three timeouts on a dead server.
    pub fn record_failure(&self, now: i64) -> bool {
        let streak = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if streak < NACOS_FAILURE_THRESHOLD {
            return false;
        }
        self.opened_at.store(now, Ordering::Relaxed);
        true
    }

    /// Count a confirmed answer: the streak restarts and any window goes away,
    /// because a reply is evidence the server is alive *now*.
    pub fn record_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
        self.opened_at.store(WINDOW_CLOSED, Ordering::Relaxed);
    }

    /// True while the backoff window covers `now`.
    ///
    /// The window holds for its whole backoff and lifts at its end, which is
    /// when the next unconfirmed name is allowed to call Nacos again.
    ///
    /// The streak is part of the test, not only a bookkeeping detail: a failure
    /// can be counted while a concurrent confirmed answer already lifted the
    /// window, and the counter is what says whether that answer came last. A clock
    /// that steps behind the window's start (an NTP correction) gives a negative
    /// elapsed time, which is read as "not open" rather than saturating to zero:
    /// holding a refusing window open past its deadline on a clock skew costs more
    /// than the one failing call that re-stamps it.
    pub fn is_open(&self, now: i64) -> bool {
        let opened_at = self.opened_at.load(Ordering::Relaxed);
        let failures = self.failures.load(Ordering::Relaxed);
        if opened_at == WINDOW_CLOSED || failures < NACOS_FAILURE_THRESHOLD {
            return false;
        }
        let elapsed = now.saturating_sub(opened_at);
        elapsed >= 0 && elapsed < NACOS_FAILURE_BACKOFF.as_secs() as i64
    }
}

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

    /// True when `service_name` should be served.
    ///
    /// Case-sensitive. The only production caller passes a service label out of a
    /// `LowerName`, so that name is already lowercase here.
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
/// No instance data is kept here. A name this process has already subscribed is
/// read with `get_all_instances(subscribe = true)`, which the SDK answers from
/// its naming cache when it has an entry; because the name was subscribed when
/// it entered the ledger, pushes keep that cache current — including a push that
/// removes the last instance, which reaches DNS on the next query.
///
/// The first query for a name does not read that cache. It asks the server
/// directly (`subscribe = false`), because a cache entry the client loaded from
/// disk, or the absence of one, is not evidence about a name this process has
/// never confirmed — and the SDK's `subscribe = true` path reports a failed
/// fetch as an empty list, which would be served as authoritative NXDOMAIN.
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
    /// Consecutive failures of the calls that actually reach Nacos.
    backoff: FailureBackoff,
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
            // Load the client's naming cache from disk at start-up. A name's
            // first query is still confirmed by the server, so loaded entries
            // are not served as unconfirmed answers; they are what a subscribed
            // name reads when the server is already down again by then. It also
            // keeps a Nacos outage at boot from failing the build (the client
            // retries in the background).
            .naming_load_cache_at_start(true);

        let naming_service = NamingServiceBuilder::new(props).build().await?;

        Ok(Self {
            naming_service: Arc::new(naming_service),
            group: group.to_string(),
            filter,
            listener: Arc::new(NoopListener),
            subscribed: DashSet::new(),
            backoff: FailureBackoff::new(),
        })
    }

    /// How many names this process has subscribed to, for the control plane's
    /// per-zone live signal.
    pub fn subscribed_count(&self) -> usize {
        self.subscribed.len()
    }
}

impl ServiceCatalog for NacosServiceWatcher {
    /// The client cache's answer for `service`, or a server confirmation if this
    /// process has never subscribed the name.
    ///
    /// Bounded four ways: [`ServiceFilter`] has to select the name, the subscribed
    /// ledger keeps a `subscribe` call off the steady-state path, the wait on Nacos
    /// is capped at [`NACOS_LOOKUP_TIMEOUT`], and while the [`FailureBackoff`]
    /// window is open an unconfirmed name is refused
    /// outright. The group and namespace are not checked here: this client only
    /// ever asks the one pair it was built with, and the authority has already
    /// matched the name's labels against it.
    async fn instances(&self, service: &str) -> CatalogAnswer {
        if !self.filter.is_selected(service) {
            tracing::debug!(
                "nacos service '{}' left unsubscribed: service_pattern did not select it",
                service
            );
            return CatalogAnswer::Unknown;
        }

        // The window refuses names this process has never confirmed, and nothing
        // else: a ledger name reads the client's cache, which costs no round
        // trip, so an outage does not make it worth cutting off.
        let now = doggy_dns_basic::time::unix_now_secs();
        if self.backoff.is_open(now) && !self.subscribed.contains(service) {
            tracing::debug!(
                "nacos backoff window open; refusing '{}' with SERVFAIL and no call",
                service
            );
            return CatalogAnswer::Unavailable;
        }

        match tokio::time::timeout(NACOS_LOOKUP_TIMEOUT, self.answer_from_nacos(service)).await {
            Ok(answer) => answer,
            Err(_) => {
                tracing::warn!(
                    "nacos did not answer for service '{}' within {:?}; answering SERVFAIL",
                    service,
                    NACOS_LOOKUP_TIMEOUT
                );
                self.record_remote_failure();
                CatalogAnswer::Unavailable
            }
        }
    }
}

impl NacosServiceWatcher {
    /// The part of [`ServiceCatalog::instances`] that may reach the network:
    /// confirm an unledgered name with the server, then read the client cache.
    async fn answer_from_nacos(&self, service: &str) -> CatalogAnswer {
        if self.subscribed.contains(service) {
            return self.read_client_cache(service).await;
        }

        let confirmed = match self.confirm_with_server(service).await {
            Ok(instances) => instances,
            Err(e) => {
                tracing::warn!(
                    "nacos could not confirm service '{}': {}; answering SERVFAIL so a failure is not negative-cached as NXDOMAIN",
                    service,
                    e
                );
                self.record_remote_failure();
                return CatalogAnswer::Unavailable;
            }
        };
        // A reply is evidence the server answers, which is what the counter and
        // any window are measuring.
        self.backoff.record_success();

        // The answer is the server's own list, so it stands whether or not the
        // subscribe below armed.
        self.subscribe_name(service).await;

        Self::answer_of(service, &confirmed)
    }

    /// Ask the server for `service` with no cache read and no subscription.
    ///
    /// `subscribe = false` is the only shape nacos-sdk 0.8.3 has that reports a
    /// failed request: it forwards the error straight out of `request_to_server`.
    /// With `subscribe = true` the SDK swallows its own fetch failure and returns
    /// an empty list, which is indistinguishable from a name that does not exist.
    async fn confirm_with_server(
        &self,
        service: &str,
    ) -> Result<Vec<ServiceInstance>, nacos_sdk::api::error::Error> {
        self.naming_service
            .get_all_instances(service.to_string(), Some(self.group.clone()), vec![], false)
            .await
    }

    /// The client cache's own answer for a name this process subscribed.
    async fn read_client_cache(&self, service: &str) -> CatalogAnswer {
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

        Self::answer_of(service, &instances)
    }

    /// Subscribe `service`, then put it in the ledger.
    ///
    /// The order is the contract: a ledgered name is one the next query reads
    /// straight out of the client's cache without asking the server, so publishing
    /// the entry before the await would let a concurrent first query take that
    /// shortcut for a name the client was not yet able to answer from. A cold race
    /// can now send two subscribes; the SDK replaces an identical listener rather
    /// than adding it twice, and neither query answers before its own server
    /// confirmation.
    async fn subscribe_name(&self, service: &str) {
        // `contains` reads the `&str` this already has. Going straight to
        // `insert` would allocate a `String` on every query just to find out the
        // name is already subscribed - which, in steady state, it always is.
        if self.subscribed.contains(service) {
            return;
        }

        let subscribed = self
            .naming_service
            .subscribe(
                service.to_string(),
                Some(self.group.clone()),
                vec![],
                Arc::clone(&self.listener),
            )
            .await;
        match subscribed {
            Ok(()) => {
                self.subscribed.insert(service.to_string());
                tracing::info!("asked the nacos client to subscribe service '{}'", service);
            }
            // Defensive. nacos-sdk 0.8.3's `subscribe` registers the listener and
            // its own retry task, then swallows the request error and returns
            // `Ok(())`, so nothing reaches this arm live. If a version does hand a
            // failure back, the name stays out of the ledger and the next query
            // asks the server again.
            Err(e) => {
                tracing::warn!(
                    "nacos subscribe of service '{}' failed: {}; the next query asks again",
                    service,
                    e
                );
            }
        }
    }

    /// What a service list implies.
    fn answer_of(service: &str, instances: &[ServiceInstance]) -> CatalogAnswer {
        let usable = usable_instances(instances);
        if usable.is_empty() {
            // Either Nacos does not know the name, or it has nothing healthy to
            // serve; the list carries no presence flag, so both answer
            // authoritative NXDOMAIN rather than NODATA.
            tracing::debug!("nacos has no usable instance of '{}'", service);
            return CatalogAnswer::Unknown;
        }
        CatalogAnswer::Instances(usable)
    }

    /// Count one failure of a call that reached Nacos, and say so when that
    /// failure opened the backoff window.
    ///
    /// Only failures are counted here, never cache reads: an answer served from
    /// the client's cache says nothing about whether the server is up. The
    /// per-query cause is logged by the caller; this is the one line that says
    /// what happens next.
    fn record_remote_failure(&self) {
        let now = doggy_dns_basic::time::unix_now_secs();
        if self.backoff.record_failure(now) {
            tracing::warn!(
                "nacos remote failures have reached the {NACOS_FAILURE_THRESHOLD}-in-a-row threshold; holding {NACOS_FAILURE_BACKOFF:?} before asking about a new name again. Names already subscribed keep answering from the client cache, unconfirmed ones get SERVFAIL."
            );
        }
    }
}
