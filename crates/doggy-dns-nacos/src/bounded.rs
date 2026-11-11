//! One bounded SDK call, shared by every source the watcher reads.

use std::future::Future;
use std::time::Duration;

/// Why a bounded call produced no value: the bound elapsed, or the call
/// itself reported an error. What each means for the answer — log, failure
/// count, fallback — stays with the caller.
#[derive(Debug)]
pub enum CallFailure<E> {
    Timeout,
    Sdk(E),
}

/// Run `call` under `bound`, flattening the timeout and the call's own error
/// into one result so each source matches on the cause in one place.
pub async fn bounded_call<T, E, F>(bound: Duration, call: F) -> Result<T, CallFailure<E>>
where
    F: Future<Output = Result<T, E>>,
{
    match tokio::time::timeout(bound, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(CallFailure::Sdk(e)),
        Err(_) => Err(CallFailure::Timeout),
    }
}
