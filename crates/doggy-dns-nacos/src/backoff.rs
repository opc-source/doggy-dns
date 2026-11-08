use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

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
