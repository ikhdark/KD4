use std::future::Future;
use std::io;

use tokio::time::Instant;
use tokio::time::sleep_until;

use crate::FileWatcher;
use crate::FileWatcherSubscriber;
use crate::Receiver;
use crate::WatchPath;
use crate::WatchRegistration;

/// A wait only succeeds when its state check returns true, never on an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Satisfied,
    TimedOut,
    Cancelled,
}

// Native registration and teardown can block. Keep both off async workers,
// including when a cancelled setup finishes after its caller has gone away.
pub struct Subscription {
    handles: Option<(WatchRegistration, FileWatcherSubscriber)>,
    receiver: Receiver,
}

impl Subscription {
    /// Install coverage before capturing a baseline. Events received during
    /// capture remain queued. This grants no filesystem read authority and
    /// does not make a multi-file baseline atomic.
    pub async fn new(paths: Vec<WatchPath>) -> io::Result<Self> {
        if paths.is_empty() || paths.iter().any(|path| !path.path.is_absolute()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file subscriptions require nonempty absolute watch paths",
            ));
        }
        tokio::task::spawn_blocking(move || {
            let watcher = FileWatcher::shared().map_err(io::Error::other)?;
            let (subscriber, receiver) = watcher.add_subscriber();
            let mut subscription = Self {
                handles: Some((WatchRegistration::default(), subscriber)),
                receiver,
            };
            let (registration, subscriber) = subscription.handles.as_mut()
                .ok_or_else(|| io::Error::other("file subscription lost its handles"))?;
            *registration = subscriber.register_paths(paths).map_err(io::Error::other)?;
            Ok(subscription)
        }).await.map_err(io::Error::other)?
    }

    /// A notification is only a reason to recheck authoritative state. In
    /// particular, rescan_required means paths may have been lost to overflow
    /// or degraded coverage, not that any particular file changed.
    pub async fn recv(&mut self) -> io::Result<crate::FileWatcherEvent> {
        self.receiver.recv().await.ok_or_else(|| io::Error::new(
            io::ErrorKind::BrokenPipe, "filesystem watcher closed",
        ))
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(handles) = self.handles.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                drop(runtime.spawn_blocking(move || drop(handles)));
            } else {
                drop(handles);
            }
        }
    }
}

/// Checks state immediately, then uses the runtime's shared filesystem watcher
/// to wake subsequent checks. Paths must be absolute; missing paths are allowed.
///
/// Subscription precedes the second check, closing the check/subscribe race.
/// Duplicate and overflow notifications are just reasons to recheck all state.
/// The absolute deadline and cancellation cover setup and every check as well
/// as idle waiting; neither is extended by a notification. The check future must
/// be cancellation safe and must not perform blocking work on the async worker.
/// Idle waiting does not poll. Concurrent waits on a runtime share one watcher,
/// but its registry holds it weakly: a wait without a concurrent user creates
/// the native watcher and its event loop, and releases them when it returns.
/// The caller must enforce filesystem permissions in `check`; this function
/// grants no read authority.
pub async fn wait_until<F, Fut>(
    paths: Vec<WatchPath>,
    deadline: Instant,
    cancelled: impl Future<Output = ()>,
    mut check: F,
) -> io::Result<WaitOutcome>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<bool>>,
{
    if paths.is_empty() || paths.iter().any(|path| !path.path.is_absolute()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file waits require nonempty absolute watch paths",
        ));
    }
    let operation = async {
        if check().await? {
            return Ok(WaitOutcome::Satisfied);
        }
        let mut subscription = Subscription::new(paths).await?;

        loop {
            // Always recheck after registration, including events that occurred
            // before native coverage was installed. The receiver retains events
            // arriving during a check, so a false check cannot lose that wakeup.
            if check().await? {
                return Ok(WaitOutcome::Satisfied);
            }
            subscription.recv().await?;
        }
    };
    tokio::pin!(cancelled);
    let result = tokio::select! {
        biased;
        _ = &mut cancelled => return Ok(WaitOutcome::Cancelled),
        _ = sleep_until(deadline) => return Ok(WaitOutcome::TimedOut),
        result = operation => result,
    };
    // A ready check can consume CPU without yielding. Do not publish success
    // after cancellation or expiry that happened while that check ran.
    tokio::select! {
        biased;
        _ = &mut cancelled => Ok(WaitOutcome::Cancelled),
        _ = sleep_until(deadline) => Ok(WaitOutcome::TimedOut),
        result = std::future::ready(result) => result,
    }
}
