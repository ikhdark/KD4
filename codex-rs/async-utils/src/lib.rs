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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::task;

    #[tokio::test]
    async fn returns_ok_when_future_completes_first() {
        let token = CancellationToken::new();
        let value = async { 42 };

        let result = value.or_cancel(&token).await;

        assert_eq!(Ok(42), result);
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "This cancellation regression deliberately holds a guard across await to verify it is dropped"
    )]
    async fn returns_err_when_token_cancelled_first() {
        let token = CancellationToken::new();
        let child = token.child_token();
        let token_clone = token.clone();
        let lock = tokio::sync::Mutex::new(());
        let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();

        // Cancel only once the wrapped future is in flight; it never finishes on
        // its own, so the outcome does not depend on scheduler timing.
        let cancel_handle = task::spawn(async move {
            polled_rx.await.expect("wrapped future should be polled");
            token_clone.cancel();
        });

        let result = async {
            let _guard = lock.lock().await;
            let _ = polled_tx.send(());
            std::future::pending::<i32>().await
        }
        .or_cancel(&child)
        .await;

        cancel_handle.await.expect("cancel task panicked");
        assert_eq!(Err(CancelErr::Cancelled), result);
        assert!(
            lock.try_lock().is_ok(),
            "cancellation must drop owned guards"
        );
    }

    #[tokio::test]
    async fn returns_err_when_token_already_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        let result = tx.send(5).or_cancel(&token).await;

        assert_eq!(Err(CancelErr::Cancelled), result);
        assert_eq!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
    }

}
