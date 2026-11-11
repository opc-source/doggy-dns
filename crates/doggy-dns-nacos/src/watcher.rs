use crate::authority::{CatalogAnswer, ServiceCatalog};
use crate::bounded::{CallFailure, bounded_call};
use dashmap::DashSet;
use nacos_sdk::api::naming::{
    NamingChangeEvent, NamingEventListener, NamingService, NamingServiceBuilder, ServiceInstance,
};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;
use std::time::Duration;

pub use crate::backoff::{FailureBackoff, NACOS_FAILURE_BACKOFF, NACOS_FAILURE_THRESHOLD};
pub use crate::filter::ServiceFilter;
pub use crate::instances::{CachedInstance, cache_hit_of, usable_instances};

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
/// (`naming/mod.rs:495-527`), so the name stays subscribed and the SDK fills the
/// cache itself once the registry answers again. What it buys is the point: a
/// start-up or outage probe of a name the client has never seen costs 50 ms instead
/// of a full [`NACOS_LOOKUP_TIMEOUT`].
pub const NACOS_CACHE_READ_TIMEOUT: Duration = Duration::from_millis(50);

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
                "nacos service '{}' left unsubscribed: the service filter did not select it",
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
        let confirmed = match bounded_call(
            NACOS_LOOKUP_TIMEOUT,
            self.query_naming_service(service, false),
        )
        .await
        {
            Ok(instances) => instances,
            Err(failure) => {
                match failure {
                    CallFailure::Sdk(e) => {
                        tracing::warn!(
                            "nacos could not confirm service '{}': {}; answering SERVFAIL so a failure is not negative-cached as NXDOMAIN",
                            service,
                            e
                        );
                    }
                    CallFailure::Timeout => {
                        tracing::warn!(
                            "nacos did not confirm service '{}' within {:?}; answering SERVFAIL so a hang is not negative-cached as NXDOMAIN",
                            service,
                            NACOS_LOOKUP_TIMEOUT
                        );
                    }
                }
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

        cache_hit_of(service, &confirmed).unwrap_or(CatalogAnswer::Unknown)
    }

    /// What the client's own view answers for `service` right now, if anything.
    ///
    /// [`NACOS_CACHE_READ_TIMEOUT`] is the whole budget, and this never counts a
    /// failure: a cache that has nothing to say says nothing about the server. The
    /// caller decides what a `None` means - ask the server, or refuse.
    ///
    /// A hit is what arms the subscription. `get_all_instances` returns the cached
    /// entry before `subscribe_async` is reached (`naming/mod.rs:373-381`), so
    /// without the call below a disk-loaded entry would never be refreshed by a
    /// push or the polling updater, and the name would stay out of the ledger to be
    /// probed again on every query.
    async fn answer_from_cache(&self, service: &str) -> Option<CatalogAnswer> {
        let instances = match bounded_call(
            NACOS_CACHE_READ_TIMEOUT,
            self.query_naming_service(service, true),
        )
        .await
        {
            Ok(instances) => instances,
            Err(CallFailure::Sdk(e)) => {
                tracing::debug!(
                    "the nacos client could not read service '{}': {}",
                    service,
                    e
                );
                return None;
            }
            Err(CallFailure::Timeout) => return None,
        };

        let hit = cache_hit_of(service, &instances);
        if hit.is_some() {
            self.subscribe_name(service).await;
        }
        hit
    }

    /// Ask the SDK for `service`, reading its naming cache first when
    /// `subscribe` is set.
    ///
    /// `subscribe = false` is the only shape nacos-sdk has that reports a
    /// failed request: it forwards the error straight out of `request_to_server`,
    /// touching neither the cache nor the subscription registry, which is what
    /// makes the answer a confirmation.
    ///
    /// With `subscribe = true` the SDK reads the naming cache when it holds the
    /// name; when it does not, it subscribes the name and waits for its first
    /// fetch, which is why every caller bounds this call rather than trusting it
    /// to be local. A failed fetch is swallowed into an empty list, which is
    /// indistinguishable from a name that does not exist.
    ///
    /// The list comes back as an `Arc`: the cache hands out the snapshot it
    /// already holds rather than a copy of it, so a cache hit costs a pointer
    /// bump instead of a deep clone of every instance.
    async fn query_naming_service(
        &self,
        service: &str,
        subscribe: bool,
    ) -> Result<Arc<Vec<ServiceInstance>>, nacos_sdk::api::error::Error> {
        self.naming_service
            .get_all_instances(service, Some(self.group.clone()), vec![], subscribe)
            .await
    }

    /// The client cache's own answer for a name this process subscribed.
    ///
    /// A ledgered name is read locally in the common case, but the cache only ever
    /// gets an entry from a push or the polling updater, so a name subscribed
    /// moments ago can still miss; the SDK then re-fetches inline, which is a call
    /// that reaches Nacos and is bounded and counted like any other.
    async fn read_client_cache(&self, service: &str) -> CatalogAnswer {
        let instances = match bounded_call(
            NACOS_LOOKUP_TIMEOUT,
            self.query_naming_service(service, true),
        )
        .await
        {
            Ok(instances) => instances,
            Err(CallFailure::Sdk(e)) => {
                tracing::warn!(
                    "failed to read the nacos client cache for service '{}': {}",
                    service,
                    e
                );
                return CatalogAnswer::Unavailable;
            }
            Err(CallFailure::Timeout) => {
                tracing::warn!(
                    "the nacos client did not answer for subscribed service '{}' within {:?}; answering SERVFAIL",
                    service,
                    NACOS_LOOKUP_TIMEOUT
                );
                self.record_remote_failure();
                return CatalogAnswer::Unavailable;
            }
        };

        cache_hit_of(service, &instances).unwrap_or(CatalogAnswer::Unknown)
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
                service,
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
            // Defensive. The SDK's `subscribe` registers the listener and
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
