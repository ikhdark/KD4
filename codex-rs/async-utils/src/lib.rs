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
    use std::time::Duration;
    use tokio::task;
    use tokio::time::sleep;

    #[tokio::test]
    async fn returns_ok_when_future_completes_first() {
        let token = CancellationToken::new();
        let value = async { 42 };

        let result = value.or_cancel(&token).await;

        assert_eq!(Ok(42), result);
    }

    #[tokio::test]
    async fn returns_err_when_token_cancelled_first() {
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();

        // Cancel only once the wrapped future is in flight; it never finishes on
        // its own, so the outcome does not depend on scheduler timing.
        let cancel_handle = task::spawn(async move {
            polled_rx.await.expect("wrapped future should be polled");
            token_clone.cancel();
        });

        let result = async {
            let _ = polled_tx.send(());
            std::future::pending::<i32>().await
        }
        .or_cancel(&token)
        .await;

        cancel_handle.await.expect("cancel task panicked");
        assert_eq!(Err(CancelErr::Cancelled), result);
    }

    #[tokio::test]
    async fn returns_err_when_token_already_cancelled() {
        let token = CancellationToken::new();
        token.cancel();

        let result = async {
            sleep(Duration::from_millis(50)).await;
            5
        }
        .or_cancel(&token)
        .await;

        assert_eq!(Err(CancelErr::Cancelled), result);
    }

    #[tokio::test]
    async fn cancellation_wins_when_both_are_ready() {
        let token = CancellationToken::new();
        token.cancel();

        let result = async { 5 }.or_cancel(&token).await;

        assert_eq!(Err(CancelErr::Cancelled), result);
    }
}
