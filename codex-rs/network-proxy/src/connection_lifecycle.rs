use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::graceful::Shutdown;
use rama_core::graceful::ShutdownGuard;
use rama_core::graceful::WeakShutdownGuard;

/// The sole sender is owned by the runtime handle. Drop signals cancellation
/// synchronously, including when a shutdown/wait future is abandoned.
pub(crate) struct ConnectionLifecycle {
    signal: Option<tokio::sync::oneshot::Sender<()>>,
    shutdown: Shutdown,
}

impl ConnectionLifecycle {
    pub(crate) fn new() -> Self {
        let (signal, receive) = tokio::sync::oneshot::channel();
        Self {
            signal: Some(signal),
            shutdown: Shutdown::new(async move {
                let _ = receive.await;
            }),
        }
    }

    pub(crate) fn guard(&self) -> ShutdownGuard {
        self.shutdown.guard()
    }

    pub(crate) fn cancel(&mut self) {
        self.signal.take();
    }

    pub(crate) async fn shutdown(mut self) {
        self.cancel();
        self.shutdown.shutdown().await;
    }
}

#[derive(Clone)]
pub(crate) struct CancelOnShutdown<S> {
    inner: S,
    guard: WeakShutdownGuard,
}

impl<S> CancelOnShutdown<S> {
    pub(crate) fn new(inner: S, guard: &ShutdownGuard) -> Self {
        Self {
            inner,
            guard: guard.clone_weak(),
        }
    }
}

impl<S, R> Service<R> for CancelOnShutdown<S>
where
    S: Service<R>,
    S::Error: Into<BoxError>,
    R: Send + 'static,
{
    type Output = S::Output;
    type Error = BoxError;

    async fn serve(&self, request: R) -> Result<Self::Output, Self::Error> {
        tokio::select! {
            biased;
            _ = self.guard.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "proxy shutdown").into()),
            result = self.inner.serve(request) => result.map_err(Into::into),
        }
    }
}
