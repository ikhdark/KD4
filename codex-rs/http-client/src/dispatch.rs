//! Request-local admission after asynchronous route and connection preparation.

use std::future::Future;
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;

/// Orders the first poll of a network operation against an authority transition.
pub trait RequestDispatchGuard: Send + Sync {
    /// Invoke `dispatch` exactly once under the authority's synchronization, or
    /// reject without invoking it. The callback must not re-enter that authority.
    /// No guard or lock may survive this synchronous call.
    fn admit(&self, dispatch: &mut dyn FnMut()) -> Result<(), String>;
}

tokio::task_local! {
    static DISPATCH_GUARD: Arc<Mutex<Option<Arc<dyn RequestDispatchGuard>>>>;
}

/// Start one logical request, retaining its first authority across retries and
/// transport fallback. Independent requests get independent scope state.
pub fn with_request_dispatch_scope<F: Future>(future: F) -> impl Future<Output = F::Output> {
    // Keep transport-sized futures out of both this wrapper's captured state
    // and Tokio's task-local scope adapter. Polling and guard ownership remain
    // on the caller's task; cancellation still drops this same future.
    let future = Box::pin(future);
    async move {
        DISPATCH_GUARD
            .scope(Arc::new(Mutex::new(None)), future)
            .await
    }
}

/// Keep the first known authority, rather than silently rebinding a retry.
/// Call before spawning work, since task-local scopes are not inherited.
pub fn bind_dispatch_guard(
    guard: Option<Arc<dyn RequestDispatchGuard>>,
) -> Option<Arc<dyn RequestDispatchGuard>> {
    DISPATCH_GUARD
        .try_with(|scope| {
            let mut bound = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if bound.is_none() {
                *bound = guard.clone();
            }
            bound.clone()
        })
        .unwrap_or(guard)
}

/// Bind physical dispatch to the enclosing logical request if present.
pub async fn with_dispatch_guard<F: Future>(
    guard: Option<Arc<dyn RequestDispatchGuard>>,
    future: F,
) -> F::Output {
    let guard = bind_dispatch_guard(guard);
    DISPATCH_GUARD
        .scope(Arc::new(Mutex::new(guard)), future)
        .await
}

/// Admit immediately before the first network poll, after route/pool awaits.
///
/// Once admitted, the operation may finish even if auth subsequently changes:
/// a dispatched request cannot be unsent. In particular, a stalled server must
/// not hold an auth lock or prevent logout. Each explicit redirect is admitted
/// separately; transport-managed redirects belong to the admitted operation.
pub async fn admit_dispatch<F: Future>(future: F) -> Result<F::Output, String> {
    let guard = bind_dispatch_guard(None);
    let Some(guard) = guard else {
        return Ok(future.await);
    };
    let mut future = std::pin::pin!(future);
    let mut admitted = false;
    poll_fn(move |context| {
        if admitted {
            return future.as_mut().poll(context).map(Ok);
        }
        let mut result = None;
        if let Err(error) = guard.admit(&mut || {
            result = Some(future.as_mut().poll(context));
        }) {
            return Poll::Ready(Err(error));
        }
        admitted = true;
        match result {
            Some(result) => result.map(Ok),
            None => Poll::Ready(Err("dispatch guard did not admit the operation".into())),
        }
    })
    .await
}

