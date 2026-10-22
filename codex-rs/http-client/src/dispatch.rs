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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    struct EpochGuard(Arc<RwLock<u64>>, u64);

    impl RequestDispatchGuard for EpochGuard {
        fn admit(&self, dispatch: &mut dyn FnMut()) -> Result<(), String> {
            let epoch = self.0.read().unwrap();
            if *epoch != self.1 {
                return Err("authority changed".into());
            }
            dispatch();
            Ok(())
        }
    }

    #[tokio::test]
    async fn admission_rejects_stale_work_without_polling_network() {
        let epoch = Arc::new(RwLock::new(1));
        let guard = Arc::new(EpochGuard(epoch, 0));
        let mut polled = false;
        let result = with_dispatch_guard(
            Some(guard),
            admit_dispatch(async {
                polled = true;
            }),
        )
        .await;
        assert_eq!(result, Err("authority changed".into()));
        assert!(!polled);
    }

    #[tokio::test]
    async fn stalled_admitted_work_does_not_hold_the_authority_lock() {
        let epoch = Arc::new(RwLock::new(0));
        let guard = Arc::new(EpochGuard(Arc::clone(&epoch), 0));
        let (send, receive) = tokio::sync::oneshot::channel();
        let future = with_dispatch_guard(
            Some(guard),
            admit_dispatch(async { receive.await.unwrap() }),
        );
        let mut future = std::pin::pin!(future);
        poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        // A blocked response must not block logout or require the response to finish.
        *epoch.try_write().expect("admission lock must be released") = 1;
        send.send(42).unwrap();
        assert_eq!(future.await, Ok(42));
    }

    #[tokio::test]
    async fn cancelled_scope_does_not_leak_into_an_unrelated_request() {
        let guard = Arc::new(EpochGuard(Arc::new(RwLock::new(1)), 0));
        let blocked = with_dispatch_guard(Some(guard), std::future::pending::<()>());
        let mut blocked = std::pin::pin!(blocked);
        poll_fn(|context| {
            assert!(blocked.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(admit_dispatch(async { 7 }).await, Ok(7));
    }

    #[tokio::test]
    async fn logical_retry_cannot_adopt_a_replacement_accounts_guard() {
        let epoch = Arc::new(RwLock::new(0));
        let initial: Arc<dyn RequestDispatchGuard> = Arc::new(EpochGuard(Arc::clone(&epoch), 0));
        with_request_dispatch_scope(async {
            assert_eq!(
                with_dispatch_guard(Some(initial), admit_dispatch(async { 1 })).await,
                Ok(1)
            );
            *epoch.write().unwrap() = 1;
            let replacement = Arc::new(EpochGuard(Arc::clone(&epoch), 1));
            let mut dispatched = false;
            let result = with_dispatch_guard(
                Some(replacement),
                admit_dispatch(async {
                    dispatched = true;
                }),
            )
            .await;
            assert_eq!(result, Err("authority changed".into()));
            assert!(!dispatched);
        })
        .await;
    }
}
