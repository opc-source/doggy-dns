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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

/// The built-in subscription rule: a bare application name — ASCII letters,
/// digits, `_`, `-`, and no dots.
///
/// Spelled as an explicit class rather than `\w`: with the regex crate's default
/// `unicode` feature `\w` also matches CJK, so writing the rule that way would
/// quietly widen the built-in default to names an operator did not ask for.
/// Configured patterns keep that same Unicode behaviour, deliberately.
const DEFAULT_SERVICE_PATTERN: &str = "[A-Za-z0-9_-]+";

/// How long one call that reaches Nacos may wait before it answers SERVFAIL.
///
/// The SDK retries a dead connection in the background and leaves a request
/// pending until the connection returns, which is longer than any DNS client
/// waits. The bound turns that wait into an answer the client can act on.
///
/// It is a per-call bound, not a per-query one: a query that first reads the
/// client's cache and then asks the server gets both budgets, so its worst case
/// is [`NACOS_CACHE_READ_TIMEOUT`] plus this.
pub const NACOS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the client's own view may take to answer.
///
/// A name the cache holds answers in microseconds: it is a map read and a clone.
/// So this bound only ever cuts off the round trip the SDK makes when the name is
/// *not* held, and cutting that one off costs nothing. `subscribe_async` registers
/// the listener, the refresh task and the redo task before it waits for the server
/// (`naming/mod.rs:489-520`), so the name stays subscribed and the SDK fills the
/// cache itself once the registry answers again. What it buys is the point: a
/// start-up or outage probe of a name the client has never seen costs 50 ms instead
/// of a full [`NACOS_LOOKUP_TIMEOUT`].
pub const NACOS_CACHE_READ_TIMEOUT: Duration = Duration::from_millis(50);

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
    /// Whether a call that reached Nacos ever answered in this process.
    ///
    /// Set once by [`FailureBackoff::record_success`] and never cleared: this is
    /// not a health check, it is the memory that the registry has been proven
    /// reachable. It decides whether an unledgered name may be answered from the
    /// client's cache before anyone has asked the server.
    confirmed: AtomicBool,
}

impl Default for FailureBackoff {
    fn default() -> Self {
        Self {
            failures: AtomicU32::new(0),
            opened_at: AtomicI64::new(WINDOW_CLOSED),
            confirmed: AtomicBool::new(false),
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
        self.confirmed.store(true, Ordering::Relaxed);
    }

    /// Whether a call that reached Nacos has ever answered in this process.
    ///
    /// False only at the very start, which is the state where the client's cache
    /// is the only thing that can answer while the registry is out of reach. A
    /// later failure streak does not take the evidence back: the question this
    /// answers is "has this registry ever been reached", not "is it reachable now"
    /// - and the second one is [`FailureBackoff::is_open`].
    pub fn ever_confirmed(&self) -> bool {
        self.confirmed.load(Ordering::Relaxed)
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

/// Which source a query reads first, and what it may fall back to.
///
/// The three inputs are everything the watcher knows about the name and about
/// Nacos: whether this process registered a subscription for the name, whether
/// the registry has ever answered it, and whether the failure window is open.
/// Keeping that as a table rather than as branches spread across the call sites
/// is what lets the rules be read - and tested - as one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryEntry {
    /// The name is subscribed: read the client's cache. Pushes keep it current, so
    /// it is the freshest source there is, and it answers locally even while the
    /// registry is down. A name whose entry has not been written yet is the one
    /// case where this read reaches the server, and it keeps the full
    /// [`NACOS_LOOKUP_TIMEOUT`] for that.
    ClientCache,
    /// Not subscribed, and the registry has never answered this process: the
    /// client's cache first. An entry it holds answers a start-up outage; an empty
    /// list is not served, so the name still gets its server confirmation.
    CacheFirst,
    /// Not subscribed, and the registry does answer: ask it, and read the client's
    /// cache only after that call failed. A disk-loaded entry is not evidence
    /// about a name this process has never confirmed, and serving it ahead of a
    /// live server would hold the previous run's addresses for a TTL.
    ServerFirst,
    /// Not subscribed and the failure window is open: the client's cache is the
    /// only source, and a name it does not hold is refused without waiting on the
    /// registry.
    CacheOnly,
}

/// The table behind [`QueryEntry`]. See the variants for what each state means
/// and why it reads what it reads.
pub fn entry_of(ledgered: bool, ever_confirmed: bool, window_open: bool) -> QueryEntry {
    if ledgered {
        QueryEntry::ClientCache
    } else if window_open {
        QueryEntry::CacheOnly
    } else if ever_confirmed {
        QueryEntry::ServerFirst
    } else {
        QueryEntry::CacheFirst
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

/// What the client's cache can answer for `service`, if anything.
///
/// Some only when the list holds an instance worth serving. An empty list is
/// deliberately not an answer: `get_all_instances(subscribe = true)` reports a
/// fetch that failed exactly as it reports a service with no instances
/// (`naming/mod.rs:368-372`), so from this call shape empty proves nothing, and
/// answering NXDOMAIN on it would negative-cache an outage. What a `None` means is
/// the caller's decision - ask the server, or refuse.
pub fn cache_hit_of(service: &str, instances: &[ServiceInstance]) -> Option<CatalogAnswer> {
    let usable = usable_instances(instances);
    if usable.is_empty() {
        tracing::debug!(
            "the nacos client cache has nothing to answer '{}' with",
            service
        );
        return None;
    }
    Some(CatalogAnswer::Instances(usable))
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
/// The first query for a name asks the server (`subscribe = false`) unless the
/// registry has not answered this process yet. Neither an entry the client loaded
/// from disk nor the absence of one is evidence about a name this process has
/// never confirmed, and the SDK's `subscribe = true` path reports a failed fetch
/// exactly as it reports a service with no usable instance, so an answer taken
/// only from the cache can turn an outage into an authoritative NXDOMAIN. That is
/// why the server is asked by default. But while no reply has ever arrived, the
/// confirmation has nothing to give and the client's own view is what stands
/// between a start-up outage and SERVFAIL for every name — so there the cache is
/// read first, briefly, and an empty list is still not served. [`entry_of`] is
/// the table of those rules.
///
/// A call that reaches the server and fails, or one that outlives the bound, is
/// counted by [`FailureBackoff`]; the window it opens leaves the client's cache as
/// the only source for names not yet subscribed.
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
            // Load the client's naming cache from disk at start-up. What a loaded
            // entry answers is a name the registry has not been reached yet, so it
            // is the emergency view — never a substitute for the confirmation a
            // live server gives afterwards. It also keeps a Nacos outage at boot
            // from failing the build (the client retries in the background).
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
    /// What this process reads to answer for `service`, chosen by [`entry_of`].
    ///
    /// Bounded four ways: [`ServiceFilter`] has to select the name, the subscribed
    /// ledger keeps a `subscribe` call off the steady-state path, every call that
    /// reaches Nacos is capped at [`NACOS_LOOKUP_TIMEOUT`] and every cache read
    /// taken instead of a call at [`NACOS_CACHE_READ_TIMEOUT`], and while the
    /// [`FailureBackoff`] window is open a name the client cache does not hold is
    /// refused outright. The group and namespace are not checked here: this client
    /// only ever asks the one pair it was built with, and the authority has already
    /// matched the name's labels against it.
    async fn instances(&self, service: &str) -> CatalogAnswer {
        if !self.filter.is_selected(service) {
            tracing::debug!(
                "nacos service '{}' left unsubscribed: service_pattern did not select it",
                service
            );
            return CatalogAnswer::Unknown;
        }

        let now = doggy_dns_basic::time::unix_now_secs();
        let entry = entry_of(
            self.subscribed.contains(service),
            self.backoff.ever_confirmed(),
            self.backoff.is_open(now),
        );

        match entry {
            // Subscribed: the cache is the freshest source, so an outage does not
            // make it worth cutting off.
            QueryEntry::ClientCache => self.read_client_cache(service).await,
            // Nothing has ever come back from the registry, so the client's own
            // view is the only thing that can answer quickly - and a name it does
            // not hold still gets its required confirmation.
            QueryEntry::CacheFirst => match self.answer_from_cache(service).await {
                Some(answer) => answer,
                None => self.answer_from_server(service).await,
            },
            // The registry answers, so it is asked; the client's cache only gets a
            // say once that call has failed.
            QueryEntry::ServerFirst => match self.answer_from_server(service).await {
                CatalogAnswer::Unavailable => self
                    .answer_from_cache(service)
                    .await
                    .unwrap_or(CatalogAnswer::Unavailable),
                answer => answer,
            },
            // The window is open and the name is not subscribed: nothing is worth
            // a wait on the registry, and what the client holds is all there is.
            QueryEntry::CacheOnly => match self.answer_from_cache(service).await {
                Some(answer) => answer,
                None => {
                    tracing::debug!(
                        "nacos backoff window open and the client cache does not hold '{}'; answering SERVFAIL without waiting on the registry",
                        service
                    );
                    CatalogAnswer::Unavailable
                }
            },
        }
    }
}

impl NacosServiceWatcher {
    /// Ask the server for `service`, then answer from its own list.
    ///
    /// The confirmation is the rule that keeps a name this process has never seen
    /// from being answered by absence: only the server can tell "unknown service"
    /// apart from "the fetch failed", and only `subscribe = false` reports the
    /// failure. A name that gets here and answers is therefore served from the
    /// server's own list, and the reply is what credits the registry for
    /// [`entry_of`] from then on.
    async fn answer_from_server(&self, service: &str) -> CatalogAnswer {
        let confirmed = match tokio::time::timeout(
            NACOS_LOOKUP_TIMEOUT,
            self.confirm_with_server(service),
        )
        .await
        {
            Ok(Ok(instances)) => instances,
            Ok(Err(e)) => {
                tracing::warn!(
                    "nacos could not confirm service '{}': {}; answering SERVFAIL so a failure is not negative-cached as NXDOMAIN",
                    service,
                    e
                );
                self.record_remote_failure();
                return CatalogAnswer::Unavailable;
            }
            Err(_) => {
                tracing::warn!(
                    "nacos did not confirm service '{}' within {:?}; answering SERVFAIL so a hang is not negative-cached as NXDOMAIN",
                    service,
                    NACOS_LOOKUP_TIMEOUT
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

    /// What the client's own view answers for `service` right now, if anything.
    ///
    /// [`NACOS_CACHE_READ_TIMEOUT`] is the whole budget, and this never counts a
    /// failure: a cache that has nothing to say says nothing about the server. The
    /// caller decides what a `None` means - ask the server, or refuse.
    ///
    /// A hit is what arms the subscription. `get_all_instances` returns the cached
    /// entry before `subscribe_async` is reached (`naming/mod.rs:361-363`), so
    /// without the call below a disk-loaded entry would never be refreshed by a
    /// push or the polling updater, and the name would stay out of the ledger to be
    /// probed again on every query.
    async fn answer_from_cache(&self, service: &str) -> Option<CatalogAnswer> {
        let instances =
            match tokio::time::timeout(NACOS_CACHE_READ_TIMEOUT, self.fetch_cache(service)).await {
                Ok(Ok(instances)) => instances,
                Ok(Err(e)) => {
                    tracing::debug!(
                        "the nacos client could not read service '{}': {}",
                        service,
                        e
                    );
                    return None;
                }
                Err(_) => return None,
            };

        let hit = cache_hit_of(service, &instances);
        if hit.is_some() {
            self.subscribe_name(service).await;
        }
        hit
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

    /// Ask the client's own view for `service`.
    ///
    /// `subscribe = true` reads the naming cache when it holds the name; when it
    /// does not, the SDK subscribes the name and waits for its first fetch, which is
    /// why every caller bounds this call rather than trusting it to be local.
    async fn fetch_cache(
        &self,
        service: &str,
    ) -> Result<Vec<ServiceInstance>, nacos_sdk::api::error::Error> {
        self.naming_service
            .get_all_instances(
                service.to_string(),
                Some(self.group.clone()),
                vec![],
                // Reads the client's cache when it has an entry, which is what
                // keeps the common query local.
                true,
            )
            .await
    }

    /// The client cache's own answer for a name this process subscribed.
    ///
    /// A ledgered name is read locally in the common case, but the cache only ever
    /// gets an entry from a push or the polling updater, so a name subscribed
    /// moments ago can still miss; the SDK then re-fetches inline, which is a call
    /// that reaches Nacos and is bounded and counted like any other.
    async fn read_client_cache(&self, service: &str) -> CatalogAnswer {
        let instances = match tokio::time::timeout(NACOS_LOOKUP_TIMEOUT, self.fetch_cache(service))
            .await
        {
            Ok(Ok(instances)) => instances,
            Ok(Err(e)) => {
                tracing::warn!(
                    "failed to read the nacos client cache for service '{}': {}",
                    service,
                    e
                );
                return CatalogAnswer::Unavailable;
            }
            Err(_) => {
                tracing::warn!(
                    "the nacos client did not answer for subscribed service '{}' within {:?}; answering SERVFAIL",
                    service,
                    NACOS_LOOKUP_TIMEOUT
                );
                self.record_remote_failure();
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
    /// than adding it twice, and a query that races ahead of the entry takes
    /// whichever path [`entry_of`] gives an unledgered name.
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
                "nacos remote failures have reached the {NACOS_FAILURE_THRESHOLD}-in-a-row threshold; holding {NACOS_FAILURE_BACKOFF:?} before asking about a new name again. Names already subscribed, and any the client cache holds, keep answering from it; the rest get SERVFAIL."
            );
        }
    }
}
