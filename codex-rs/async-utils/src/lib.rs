use std::future::Future;
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
pub enum CancelErr {
    Cancelled,
}

pub trait OrCancelExt: Sized {
    type Output;

    /// Returns when either this future completes or the token is cancelled.
    ///
    /// Cancellation wins if both are ready. An already-cancelled token prevents
    /// the wrapped future from being polled, so it cannot start obsolete work.
    /// This does not abort spawned tasks or wait for asynchronous cleanup.
    ///
    /// Cancellation is implemented by dropping the wrapped future, so the
    /// wrapped operation must be cancellation-safe. Resource-owning operations
    /// that require cleanup should handle cancellation internally and be
    /// awaited until that cleanup completes instead of using this method.
    fn or_cancel(
        self,
        token: &CancellationToken,
    ) -> impl Future<Output = Result<Self::Output, CancelErr>> + Send;
}

impl<F> OrCancelExt for F
where
    F: Future + Send,
    F::Output: Send,
{
    type Output = F::Output;

    async fn or_cancel(self, token: &CancellationToken) -> Result<Self::Output, CancelErr> {
        tokio::select! {
            biased;

            _ = token.cancelled() => Err(CancelErr::Cancelled),
            res = self => Ok(res),
        }
    }
}

