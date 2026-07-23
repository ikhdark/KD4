//! Watches subscribed files or directories and routes coarse-grained change
//! notifications to the subscribers that own matching watched paths.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use notify::Event;
use notify::EventKind;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::warn;

const RAW_EVENT_BUFFER_CAPACITY: usize = 1024;
const SUBSCRIBER_PATH_BUFFER_CAPACITY: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Coalesced file change notification for a subscriber.
pub struct FileWatcherEvent {
    /// Changed paths delivered in sorted order with duplicates removed.
    pub paths: Vec<PathBuf>,
    /// Whether one or more paths could not be retained and the subscriber must
    /// rescan its watched state instead of relying only on `paths`.
    pub rescan_required: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
/// Path subscription registered by a [`FileWatcherSubscriber`].
pub struct WatchPath {
    /// Root path to watch.
    pub path: PathBuf,
    /// Whether events below `path` should match recursively.
    pub recursive: bool,
}

type SubscriberId = u64;

#[derive(Default)]
struct WatchState {
    next_subscriber_id: SubscriberId,
    path_ref_counts: HashMap<PathBuf, PathWatchCounts>,
    subscribers: HashMap<SubscriberId, SubscriberState>,
}

struct SubscriberState {
    watched_paths: HashMap<SubscriberWatchKey, SubscriberWatchState>,
    tx: WatchSender,
}

/// Stable per-subscriber watch identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SubscriberWatchKey {
    /// Original path requested by the subscriber. Notifications are reported
    /// in this namespace so clients do not see canonicalization artifacts.
    requested: WatchPath,
}

/// Mutable per-subscriber watch state.
struct SubscriberWatchState {
    /// Existing path passed to the OS watcher and used for ref-counting. This
    /// is usually `requested`, but missing targets use an existing ancestor.
    actual: WatchPath,
    /// Current canonical equivalent of `requested` used to match backend
    /// events. This can change when an initially missing path appears through
    /// a symlink or another canonical namespace.
    matched: WatchPath,
    count: usize,
    /// Whether the requested path existed the last time an ancestor event was
    /// handled. This preserves delete notifications for fallback watches.
    last_exists: bool,
    /// Whether this watch started from a missing path. Such watches normalize
    /// ancestor create/delete events back to `requested`.
    fallback: bool,
}

/// Registration-time watch data before it is merged into subscriber state.
///
/// The key is stable for unregistering while `actual` may later move closer
/// to the requested path as missing path components are created.
#[derive(Clone)]
struct SubscriberWatchRegistration {
    /// Immutable subscriber-visible identity for this registration.
    key: SubscriberWatchKey,
    /// Existing path initially passed to the OS watcher.
    actual: WatchPath,
    /// Canonical path namespace initially used for event matching.
    matched: WatchPath,
    /// Whether registration started from a missing path fallback.
    fallback: bool,
}

/// Receives coalesced change notifications for a single subscriber.
pub struct Receiver {
    inner: Arc<ReceiverInner>,
}

struct WatchSender {
    inner: Arc<ReceiverInner>,
}

struct ReceiverInner {
    changed_paths: AsyncMutex<BTreeSet<PathBuf>>,
    rescan_required: AtomicBool,
    notify: Notify,
    sender_count: AtomicUsize,
}

impl Receiver {
    /// Waits for the next batch of changed paths, or returns `None` once the
    /// corresponding subscriber has been removed and no more events can arrive.
    pub async fn recv(&mut self) -> Option<FileWatcherEvent> {
        loop {
            let notified = self.inner.notify.notified();
            {
                let mut changed_paths = self.inner.changed_paths.lock().await;
                let rescan_required = self.inner.rescan_required.swap(false, Ordering::AcqRel);
                if rescan_required || !changed_paths.is_empty() {
                    return Some(FileWatcherEvent {
                        paths: std::mem::take(&mut *changed_paths).into_iter().collect(),
                        rescan_required,
                    });
                }
                if self.inner.sender_count.load(Ordering::Acquire) == 0 {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl WatchSender {
    async fn add_changed_paths(&self, paths: &[PathBuf]) {
        if paths.is_empty() || self.inner.rescan_required.load(Ordering::Acquire) {
            return;
        }

        let mut changed_paths = self.inner.changed_paths.lock().await;
        if self.inner.rescan_required.load(Ordering::Acquire) {
            return;
        }
        let previous_len = changed_paths.len();
        for path in paths {
            if changed_paths.len() >= SUBSCRIBER_PATH_BUFFER_CAPACITY
                && !changed_paths.contains(path)
            {
                changed_paths.clear();
                drop(changed_paths);
                self.mark_rescan_required();
                return;
            }
            changed_paths.insert(path.clone());
        }
        if changed_paths.len() != previous_len {
            self.inner.notify.notify_one();
        }
    }

    fn mark_rescan_required(&self) {
        self.inner.rescan_required.store(true, Ordering::Release);
        self.inner.notify.notify_one();
    }
}

impl Clone for WatchSender {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for WatchSender {
    fn drop(&mut self) {
        if self.inner.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.notify.notify_waiters();
        }
    }
}

fn watch_channel() -> (WatchSender, Receiver) {
    let inner = Arc::new(ReceiverInner {
        changed_paths: AsyncMutex::new(BTreeSet::new()),
        rescan_required: AtomicBool::new(false),
        notify: Notify::new(),
        sender_count: AtomicUsize::new(1),
    });
    (
        WatchSender {
            inner: Arc::clone(&inner),
        },
        Receiver { inner },
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PathWatchCounts {
    non_recursive: usize,
    recursive: usize,
}

impl PathWatchCounts {
    fn increment(&mut self, recursive: bool, amount: usize) {
        if recursive {
            self.recursive += amount;
        } else {
            self.non_recursive += amount;
        }
    }

    fn decrement(&mut self, recursive: bool, amount: usize) {
        if recursive {
            self.recursive = self.recursive.saturating_sub(amount);
        } else {
            self.non_recursive = self.non_recursive.saturating_sub(amount);
        }
    }

    fn effective_mode(self) -> Option<RecursiveMode> {
        if self.recursive > 0 {
            Some(RecursiveMode::Recursive)
        } else if self.non_recursive > 0 {
            Some(RecursiveMode::NonRecursive)
        } else {
            None
        }
    }

    fn is_empty(self) -> bool {
        self.non_recursive == 0 && self.recursive == 0
    }
}

struct FileWatcherInner {
    watcher: RecommendedWatcher,
    /// Backend watches that are believed to be active.
    watched_paths: HashMap<PathBuf, RecursiveMode>,
    /// Paths whose backend mode could not be reconciled with logical state.
    degraded_paths: HashSet<PathBuf>,
    #[cfg(test)]
    fail_next_watch: bool,
    #[cfg(test)]
    fail_next_unwatch: bool,
}

/// Coalesces bursts of watch notifications and emits at most once per interval.
pub struct ThrottledWatchReceiver {
    rx: Receiver,
    interval: Duration,
    next_allowed: Option<Instant>,
}

impl ThrottledWatchReceiver {
    /// Creates a throttling wrapper around a raw watcher [`Receiver`].
    pub fn new(rx: Receiver, interval: Duration) -> Self {
        Self {
            rx,
            interval,
            next_allowed: None,
        }
    }

    /// Receives the next event, enforcing the configured minimum delay after
    /// the previous emission.
    pub async fn recv(&mut self) -> Option<FileWatcherEvent> {
        if let Some(next_allowed) = self.next_allowed {
            sleep_until(next_allowed).await;
        }

        let event = self.rx.recv().await;
        if event.is_some() {
            self.next_allowed = Some(Instant::now() + self.interval);
        }
        event
    }
}

/// Coalesces file watcher notifications that arrive within a fixed debounce
/// window after the first event in each batch.
pub struct DebouncedWatchReceiver {
    rx: Receiver,
    interval: Duration,
    changed_paths: BTreeSet<PathBuf>,
    rescan_required: bool,
}

impl DebouncedWatchReceiver {
    /// Creates a debouncing wrapper around a raw watcher [`Receiver`].
    pub fn new(rx: Receiver, interval: Duration) -> Self {
        Self {
            rx,
            interval,
            changed_paths: BTreeSet::new(),
            rescan_required: false,
        }
    }

    /// Receives the next debounced event batch.
    pub async fn recv(&mut self) -> Option<FileWatcherEvent> {
        while self.changed_paths.is_empty() && !self.rescan_required {
            let event = self.rx.recv().await?;
            self.merge_event(event);
        }
        let deadline = Instant::now() + self.interval;

        loop {
            tokio::select! {
                event = self.rx.recv() => match event {
                    Some(event) => self.merge_event(event),
                    None => break,
                },
                _ = sleep_until(deadline) => break,
            }
        }

        Some(FileWatcherEvent {
            paths: std::mem::take(&mut self.changed_paths)
                .into_iter()
                .collect(),
            rescan_required: std::mem::take(&mut self.rescan_required),
        })
    }

    fn merge_event(&mut self, event: FileWatcherEvent) {
        self.rescan_required |= event.rescan_required;
        if self.rescan_required {
            self.changed_paths.clear();
        } else {
            for path in event.paths {
                if self.changed_paths.len() >= SUBSCRIBER_PATH_BUFFER_CAPACITY
                    && !self.changed_paths.contains(&path)
                {
                    self.changed_paths.clear();
                    self.rescan_required = true;
                    break;
                }
                self.changed_paths.insert(path);
            }
        }
    }
}

/// Handle used to register watched paths for one logical consumer.
pub struct FileWatcherSubscriber {
    id: SubscriberId,
    file_watcher: Arc<FileWatcher>,
}

impl FileWatcherSubscriber {
    /// Registers the provided paths for this subscriber and returns an RAII
    /// guard that unregisters them on drop.
    pub fn register_paths(
        &self,
        watched_paths: Vec<WatchPath>,
    ) -> notify::Result<WatchRegistration> {
        let watched_paths = dedupe_watched_paths(watched_paths)
            .into_iter()
            .map(|requested| {
                let (actual, matched, fallback) = actual_watch_path(&requested);
                let key = SubscriberWatchKey { requested };
                SubscriberWatchRegistration {
                    key,
                    actual,
                    matched,
                    fallback,
                }
            })
            .collect::<Vec<_>>();
        self.file_watcher.register_paths(self.id, &watched_paths)?;

        Ok(WatchRegistration {
            file_watcher: Arc::downgrade(&self.file_watcher),
            subscriber_id: self.id,
            watched_paths: watched_paths
                .iter()
                .map(|watch| watch.key.clone())
                .collect(),
        })
    }

    #[cfg(test)]
    pub(crate) fn register_path(&self, path: PathBuf, recursive: bool) -> WatchRegistration {
        self.register_paths(vec![WatchPath { path, recursive }])
            .expect("register path")
    }
}

impl Drop for FileWatcherSubscriber {
    fn drop(&mut self) {
        self.file_watcher.remove_subscriber(self.id);
    }
}

/// RAII guard for a set of active path registrations.
pub struct WatchRegistration {
    file_watcher: std::sync::Weak<FileWatcher>,
    subscriber_id: SubscriberId,
    watched_paths: Vec<SubscriberWatchKey>,
}

impl Default for WatchRegistration {
    fn default() -> Self {
        Self {
            file_watcher: std::sync::Weak::new(),
            subscriber_id: 0,
            watched_paths: Vec::new(),
        }
    }
}

impl Drop for WatchRegistration {
    fn drop(&mut self) {
        if let Some(file_watcher) = self.file_watcher.upgrade() {
            file_watcher.unregister_paths(self.subscriber_id, &self.watched_paths);
        }
    }
}

/// Multi-subscriber file watcher built on top of `notify`.
pub struct FileWatcher {
    inner: Option<Arc<Mutex<FileWatcherInner>>>,
    state: Arc<RwLock<WatchState>>,
}

impl FileWatcher {
    /// Creates a live filesystem watcher and starts its background event loop
    /// on the current Tokio runtime.
    pub fn new() -> notify::Result<Self> {
        let handle = Handle::try_current().map_err(|err| {
            let message = format!("no Tokio runtime available for file watcher: {err}");
            notify::Error::generic(&message)
        })?;
        let (raw_tx, raw_rx) = mpsc::channel(RAW_EVENT_BUFFER_CAPACITY);
        let raw_overflow = Arc::new(AtomicBool::new(false));
        let raw_overflow_notify = Arc::new(Notify::new());
        let callback_overflow = Arc::clone(&raw_overflow);
        let callback_overflow_notify = Arc::clone(&raw_overflow_notify);
        let watcher = notify::recommended_watcher(move |res| {
            enqueue_raw_event(&raw_tx, &callback_overflow, &callback_overflow_notify, res);
        })?;
        let inner = FileWatcherInner {
            watcher,
            watched_paths: HashMap::new(),
            degraded_paths: HashSet::new(),
            #[cfg(test)]
            fail_next_watch: false,
            #[cfg(test)]
            fail_next_unwatch: false,
        };
        let state = Arc::new(RwLock::new(WatchState::default()));
        let file_watcher = Self {
            inner: Some(Arc::new(Mutex::new(inner))),
            state,
        };
        file_watcher.spawn_event_loop(&handle, raw_rx, raw_overflow, raw_overflow_notify);
        Ok(file_watcher)
    }

    /// Creates an inert watcher that only supports test-driven synthetic
    /// notifications.
    pub fn noop() -> Self {
        Self {
            inner: None,
            state: Arc::new(RwLock::new(WatchState::default())),
        }
    }

    /// Adds a new subscriber and returns both its registration handle and its
    /// dedicated event receiver.
    pub fn add_subscriber(self: &Arc<Self>) -> (FileWatcherSubscriber, Receiver) {
        let (tx, rx) = watch_channel();
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let subscriber_id = state.next_subscriber_id;
        state.next_subscriber_id += 1;
        state.subscribers.insert(
            subscriber_id,
            SubscriberState {
                watched_paths: HashMap::new(),
                tx,
            },
        );

        let subscriber = FileWatcherSubscriber {
            id: subscriber_id,
            file_watcher: self.clone(),
        };
        (subscriber, rx)
    }

    fn register_paths(
        &self,
        subscriber_id: SubscriberId,
        watched_paths: &[SubscriberWatchRegistration],
    ) -> notify::Result<()> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner_guard: Option<std::sync::MutexGuard<'_, FileWatcherInner>> = None;
        let mut committed = Vec::new();

        for registration in watched_paths {
            let Some(subscriber) = state.subscribers.get(&subscriber_id) else {
                return Err(notify::Error::generic(
                    "file watcher subscriber was removed",
                ));
            };
            let actual = subscriber
                .watched_paths
                .get(&registration.key)
                .map(|watch| watch.actual.clone())
                .unwrap_or_else(|| registration.actual.clone());
            let previous_counts = state
                .path_ref_counts
                .get(&actual.path)
                .copied()
                .unwrap_or_default();
            let mut next_counts = previous_counts;
            next_counts.increment(actual.recursive, /*amount*/ 1);
            let next_mode = next_counts.effective_mode();

            if let Err(err) = self.reconfigure_watch(&actual.path, next_mode, &mut inner_guard) {
                Self::mark_watch_degraded(&actual.path, &mut inner_guard);
                Self::mark_subscribers_rescan_for_path(&state, &actual.path);
                for committed_watch in committed.iter().rev() {
                    self.unregister_path_locked(
                        &mut state,
                        subscriber_id,
                        committed_watch,
                        &mut inner_guard,
                    );
                }
                return Err(err);
            }

            let Some(subscriber) = state.subscribers.get_mut(&subscriber_id) else {
                return Err(notify::Error::generic(
                    "file watcher subscriber was removed",
                ));
            };
            match subscriber.watched_paths.entry(registration.key.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let watch = entry.get_mut();
                    watch.count += 1;
                    watch.fallback |= registration.fallback;
                    if watch.matched != registration.matched {
                        watch.last_exists = registration.matched.path.exists();
                        watch.matched = registration.matched.clone();
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(SubscriberWatchState {
                        actual: registration.actual.clone(),
                        matched: registration.matched.clone(),
                        count: 1,
                        last_exists: registration.matched.path.exists(),
                        fallback: registration.fallback,
                    });
                }
            }
            state
                .path_ref_counts
                .insert(actual.path.clone(), next_counts);
            committed.push(registration.key.clone());
        }

        Ok(())
    }

    fn unregister_paths(&self, subscriber_id: SubscriberId, watched_paths: &[SubscriberWatchKey]) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner_guard: Option<std::sync::MutexGuard<'_, FileWatcherInner>> = None;

        for subscriber_watch in watched_paths {
            self.unregister_path_locked(
                &mut state,
                subscriber_id,
                subscriber_watch,
                &mut inner_guard,
            );
        }
    }

    fn unregister_path_locked<'a>(
        &'a self,
        state: &mut WatchState,
        subscriber_id: SubscriberId,
        subscriber_watch: &SubscriberWatchKey,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) {
        let actual = {
            let Some(subscriber) = state.subscribers.get_mut(&subscriber_id) else {
                return;
            };
            let Some(subscriber_watch_state) = subscriber.watched_paths.get_mut(subscriber_watch)
            else {
                return;
            };
            let actual = subscriber_watch_state.actual.clone();
            subscriber_watch_state.count -= 1;
            if subscriber_watch_state.count == 0 {
                subscriber.watched_paths.remove(subscriber_watch);
            }
            actual
        };
        self.decrement_actual_watch_locked(state, &actual, 1, inner_guard);
    }

    fn remove_subscriber(&self, subscriber_id: SubscriberId) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(subscriber) = state.subscribers.remove(&subscriber_id) else {
            return;
        };

        let mut inner_guard: Option<std::sync::MutexGuard<'_, FileWatcherInner>> = None;
        for (_subscriber_watch, subscriber_watch_state) in subscriber.watched_paths {
            self.decrement_actual_watch_locked(
                &mut state,
                &subscriber_watch_state.actual,
                subscriber_watch_state.count,
                &mut inner_guard,
            );
        }
    }

    fn decrement_actual_watch_locked<'a>(
        &'a self,
        state: &mut WatchState,
        actual: &WatchPath,
        amount: usize,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) {
        let Some(previous_counts) = state.path_ref_counts.get(&actual.path).copied() else {
            return;
        };
        let mut next_counts = previous_counts;
        next_counts.decrement(actual.recursive, amount);
        let next_mode = next_counts.effective_mode();
        let reconfigure_result = self.reconfigure_watch(&actual.path, next_mode, inner_guard);

        if next_counts.is_empty() {
            state.path_ref_counts.remove(&actual.path);
        } else {
            state
                .path_ref_counts
                .insert(actual.path.clone(), next_counts);
        }

        if let Err(err) = reconfigure_result {
            warn!(
                "failed to reconfigure {} while unregistering: {err}",
                actual.path.display()
            );
            Self::mark_watch_degraded(&actual.path, inner_guard);
            Self::mark_subscribers_rescan_for_path(state, &actual.path);
        } else {
            Self::set_watch_degraded(&actual.path, next_mode, inner_guard);
        }
    }

    fn reconfigure_watch<'a>(
        &'a self,
        path: &Path,
        next_mode: Option<RecursiveMode>,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) -> notify::Result<()> {
        Self::reconfigure_watch_inner(self.inner.as_ref(), path, next_mode, inner_guard)
    }

    fn reconfigure_watch_inner<'a>(
        inner: Option<&'a Arc<Mutex<FileWatcherInner>>>,
        path: &Path,
        next_mode: Option<RecursiveMode>,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) -> notify::Result<()> {
        let Some(inner) = inner else {
            return Ok(());
        };
        if inner_guard.is_none() {
            let guard = inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *inner_guard = Some(guard);
        }
        let Some(guard) = inner_guard.as_mut() else {
            return Ok(());
        };

        let existing_mode = guard.watched_paths.get(path).copied();
        if existing_mode == next_mode && !guard.degraded_paths.contains(path) {
            guard.degraded_paths.remove(path);
            return Ok(());
        }
        if next_mode.is_some() && !path.exists() {
            let message = format!("watch path no longer exists: {}", path.display());
            return Err(notify::Error::generic(&message));
        }

        if existing_mode.is_some() {
            #[cfg(test)]
            if std::mem::take(&mut guard.fail_next_unwatch) {
                return Err(notify::Error::generic("injected unwatch failure"));
            }
            guard.watcher.unwatch(path)?;
            guard.watched_paths.remove(path);
        }

        let Some(next_mode) = next_mode else {
            guard.degraded_paths.remove(path);
            return Ok(());
        };

        #[cfg(test)]
        let watch_result = if std::mem::take(&mut guard.fail_next_watch) {
            Err(notify::Error::generic("injected watch failure"))
        } else {
            guard.watcher.watch(path, next_mode)
        };
        #[cfg(not(test))]
        let watch_result = guard.watcher.watch(path, next_mode);
        if let Err(err) = watch_result {
            if let Some(existing_mode) = existing_mode
                && let Err(restore_err) = guard.watcher.watch(path, existing_mode)
            {
                warn!(
                    "failed to restore watch {} after reconfiguration error: {restore_err}",
                    path.display()
                );
            } else if let Some(existing_mode) = existing_mode {
                guard
                    .watched_paths
                    .insert(path.to_path_buf(), existing_mode);
            }
            return Err(err);
        }
        guard.watched_paths.insert(path.to_path_buf(), next_mode);
        guard.degraded_paths.remove(path);
        Ok(())
    }

    fn set_watch_degraded(
        path: &Path,
        desired_mode: Option<RecursiveMode>,
        inner_guard: &mut Option<std::sync::MutexGuard<'_, FileWatcherInner>>,
    ) -> bool {
        let Some(guard) = inner_guard.as_mut() else {
            return false;
        };
        if guard.watched_paths.get(path).copied() == desired_mode {
            guard.degraded_paths.remove(path);
            false
        } else {
            guard.degraded_paths.insert(path.to_path_buf());
            true
        }
    }

    fn mark_watch_degraded(
        path: &Path,
        inner_guard: &mut Option<std::sync::MutexGuard<'_, FileWatcherInner>>,
    ) {
        if let Some(guard) = inner_guard.as_mut() {
            guard.degraded_paths.insert(path.to_path_buf());
        }
    }

    fn mark_subscribers_rescan_for_path(state: &WatchState, path: &Path) {
        for subscriber in state.subscribers.values() {
            if subscriber
                .watched_paths
                .values()
                .any(|watch| watch.actual.path == path)
            {
                subscriber.tx.mark_rescan_required();
            }
        }
    }

    fn apply_actual_watch_move<'a>(
        state: &mut WatchState,
        old_actual: &WatchPath,
        new_actual: &WatchPath,
        count: usize,
        inner: Option<&'a Arc<Mutex<FileWatcherInner>>>,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) -> bool {
        if old_actual == new_actual {
            return true;
        }
        if old_actual.path == new_actual.path {
            let Some(previous_counts) = state.path_ref_counts.get(&old_actual.path).copied() else {
                return false;
            };
            let mut next_counts = previous_counts;
            next_counts.decrement(old_actual.recursive, count);
            next_counts.increment(new_actual.recursive, count);
            let next_mode = next_counts.effective_mode();
            if let Err(err) =
                Self::reconfigure_watch_inner(inner, &old_actual.path, next_mode, inner_guard)
            {
                warn!(
                    "failed to change file watch mode for {}: {err}",
                    old_actual.path.display()
                );
                Self::mark_watch_degraded(&old_actual.path, inner_guard);
                Self::mark_subscribers_rescan_for_path(state, &old_actual.path);
                return false;
            }
            state
                .path_ref_counts
                .insert(old_actual.path.clone(), next_counts);
            return true;
        }

        let Some(previous_old_counts) = state.path_ref_counts.get(&old_actual.path).copied() else {
            return false;
        };
        let mut next_old_counts = previous_old_counts;
        next_old_counts.decrement(old_actual.recursive, count);
        let next_old_mode = next_old_counts.effective_mode();

        let previous_new_counts = state
            .path_ref_counts
            .get(&new_actual.path)
            .copied()
            .unwrap_or_default();
        let mut next_new_counts = previous_new_counts;
        next_new_counts.increment(new_actual.recursive, count);
        let next_new_mode = next_new_counts.effective_mode();

        if let Err(err) =
            Self::reconfigure_watch_inner(inner, &new_actual.path, next_new_mode, inner_guard)
        {
            warn!(
                "failed to move file watch to {}: {err}",
                new_actual.path.display()
            );
            Self::mark_watch_degraded(&new_actual.path, inner_guard);
            Self::mark_subscribers_rescan_for_path(state, &new_actual.path);
            return false;
        }

        let old_reconfigure_result =
            Self::reconfigure_watch_inner(inner, &old_actual.path, next_old_mode, inner_guard);

        if next_old_counts.is_empty() {
            state.path_ref_counts.remove(&old_actual.path);
        } else {
            state
                .path_ref_counts
                .insert(old_actual.path.clone(), next_old_counts);
        }
        state
            .path_ref_counts
            .insert(new_actual.path.clone(), next_new_counts);

        if let Err(err) = &old_reconfigure_result {
            warn!(
                "failed to release previous file watch {} after moving to {}: {err}",
                old_actual.path.display(),
                new_actual.path.display()
            );
        }
        if old_reconfigure_result.is_err() {
            Self::mark_watch_degraded(&old_actual.path, inner_guard);
            Self::mark_subscribers_rescan_for_path(state, &old_actual.path);
        } else {
            Self::set_watch_degraded(&old_actual.path, next_old_mode, inner_guard);
        }
        true
    }

    // Bridge `notify`'s callback-based events into the Tokio runtime and
    // notify the matching subscribers.
    fn spawn_event_loop(
        &self,
        handle: &Handle,
        mut raw_rx: mpsc::Receiver<notify::Result<Event>>,
        raw_overflow: Arc<AtomicBool>,
        raw_overflow_notify: Arc<Notify>,
    ) {
        let state = Arc::clone(&self.state);
        let inner = self.inner.as_ref().map(Arc::downgrade);
        handle.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    raw_event = raw_rx.recv() => {
                        let Some(raw_event) = raw_event else {
                            if raw_overflow.swap(false, Ordering::AcqRel) {
                                let inner = inner.as_ref().and_then(std::sync::Weak::upgrade);
                                Self::require_rescan_and_reconcile(&state, inner.as_ref()).await;
                            }
                            break;
                        };
                        if raw_overflow.swap(false, Ordering::AcqRel) {
                            let inner = inner.as_ref().and_then(std::sync::Weak::upgrade);
                            Self::require_rescan_and_reconcile(&state, inner.as_ref()).await;
                        }
                        match raw_event {
                            Ok(event) => {
                                if !is_mutating_event(&event) || event.paths.is_empty() {
                                    continue;
                                }
                                let inner = inner.as_ref().and_then(std::sync::Weak::upgrade);
                                Self::notify_subscribers(&state, inner.as_ref(), &event.paths).await;
                            }
                            Err(err) => {
                                warn!("file watcher error requiring rescan: {err}");
                                let inner = inner.as_ref().and_then(std::sync::Weak::upgrade);
                                Self::require_rescan_and_reconcile(&state, inner.as_ref()).await;
                            }
                        }
                    }
                    _ = raw_overflow_notify.notified() => {
                        if raw_overflow.swap(false, Ordering::AcqRel) {
                            let inner = inner.as_ref().and_then(std::sync::Weak::upgrade);
                            Self::require_rescan_and_reconcile(&state, inner.as_ref()).await;
                        }
                    }
                }
            }
        });
    }

    fn mark_all_subscribers_rescan(state: &RwLock<WatchState>) {
        let state = state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for subscriber in state.subscribers.values() {
            subscriber.tx.mark_rescan_required();
        }
    }

    async fn require_rescan_and_reconcile(
        state: &RwLock<WatchState>,
        inner: Option<&Arc<Mutex<FileWatcherInner>>>,
    ) {
        Self::mark_all_subscribers_rescan(state);
        Self::notify_subscribers(state, inner, &[]).await;
    }

    async fn notify_subscribers(
        state: &RwLock<WatchState>,
        inner: Option<&Arc<Mutex<FileWatcherInner>>>,
        event_paths: &[PathBuf],
    ) {
        let subscribers_to_notify: Vec<(WatchSender, Vec<PathBuf>)> = {
            let mut state = state
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut actual_watch_moves = Vec::new();
            let mut subscribers_to_notify = Vec::new();

            for (subscriber_id, subscriber) in &mut state.subscribers {
                let mut changed_paths = BTreeSet::new();
                let mut rescan_required = false;
                for (subscriber_watch, subscriber_watch_state) in &mut subscriber.watched_paths {
                    let (new_actual, new_matched, fallback) =
                        actual_watch_path(&subscriber_watch.requested);
                    for event_path in event_paths {
                        let changed_path = changed_path_for_event(
                            subscriber_watch,
                            subscriber_watch_state,
                            event_path,
                        )
                        .or_else(|| {
                            if subscriber_watch_state.matched == new_matched {
                                None
                            } else {
                                changed_path_for_matched_path(
                                    subscriber_watch,
                                    subscriber_watch_state,
                                    &new_matched,
                                    event_path,
                                )
                            }
                        });
                        if let Some(path) = changed_path
                            && !rescan_required
                        {
                            if changed_paths.len() >= SUBSCRIBER_PATH_BUFFER_CAPACITY
                                && !changed_paths.contains(&path)
                            {
                                changed_paths.clear();
                                rescan_required = true;
                            } else {
                                changed_paths.insert(path);
                            }
                        }
                    }

                    subscriber_watch_state.fallback |= fallback;
                    if subscriber_watch_state.actual == new_actual {
                        if subscriber_watch_state.matched != new_matched {
                            subscriber_watch_state.last_exists = new_matched.path.exists();
                        }
                        subscriber_watch_state.matched = new_matched;
                    } else {
                        actual_watch_moves.push((
                            *subscriber_id,
                            subscriber_watch.clone(),
                            subscriber_watch_state.actual.clone(),
                            new_actual,
                            new_matched,
                            subscriber_watch_state.count,
                        ));
                    }
                }
                if rescan_required {
                    subscriber.tx.mark_rescan_required();
                } else if !changed_paths.is_empty() {
                    subscribers_to_notify
                        .push((subscriber.tx.clone(), changed_paths.into_iter().collect()));
                }
            }

            let mut inner_guard: Option<std::sync::MutexGuard<'_, FileWatcherInner>> = None;
            for (subscriber_id, subscriber_watch, old_actual, new_actual, new_matched, count) in
                actual_watch_moves
            {
                let moved = Self::apply_actual_watch_move(
                    &mut state,
                    &old_actual,
                    &new_actual,
                    count,
                    inner,
                    &mut inner_guard,
                );
                let Some(subscriber) = state.subscribers.get_mut(&subscriber_id) else {
                    continue;
                };
                if moved {
                    if let Some(watch_state) = subscriber.watched_paths.get_mut(&subscriber_watch)
                        && watch_state.actual == old_actual
                    {
                        watch_state.actual = new_actual;
                        watch_state.last_exists = new_matched.path.exists();
                        watch_state.matched = new_matched;
                    }
                } else {
                    subscriber.tx.mark_rescan_required();
                }
            }

            subscribers_to_notify
        };

        for (subscriber, changed_paths) in subscribers_to_notify {
            subscriber.add_changed_paths(&changed_paths).await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn send_paths_for_test(&self, paths: Vec<PathBuf>) {
        Self::notify_subscribers(&self.state, self.inner.as_ref(), &paths).await;
    }

    #[cfg(test)]
    pub(crate) fn spawn_event_loop_for_test(&self, raw_rx: mpsc::Receiver<notify::Result<Event>>) {
        self.spawn_event_loop(
            &Handle::current(),
            raw_rx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Notify::new()),
        );
    }

    #[cfg(test)]
    pub(crate) fn watch_counts_for_test(&self, path: &Path) -> Option<(usize, usize)> {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .path_ref_counts
            .get(path)
            .map(|counts| (counts.non_recursive, counts.recursive))
    }
}

fn enqueue_raw_event(
    raw_tx: &mpsc::Sender<notify::Result<Event>>,
    raw_overflow: &AtomicBool,
    raw_overflow_notify: &Notify,
    event: notify::Result<Event>,
) {
    if event
        .as_ref()
        .is_ok_and(|event| event.paths.len() > SUBSCRIBER_PATH_BUFFER_CAPACITY)
    {
        raw_overflow.store(true, Ordering::Release);
        raw_overflow_notify.notify_one();
        return;
    }
    if let Err(mpsc::error::TrySendError::Full(_)) = raw_tx.try_send(event) {
        raw_overflow.store(true, Ordering::Release);
        raw_overflow_notify.notify_one();
    }
}

fn is_mutating_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

fn dedupe_watched_paths(mut watched_paths: Vec<WatchPath>) -> Vec<WatchPath> {
    watched_paths.sort_unstable_by(|a, b| {
        a.path
            .as_os_str()
            .cmp(b.path.as_os_str())
            .then(a.recursive.cmp(&b.recursive))
    });
    watched_paths.dedup();
    watched_paths
}

/// Returns the actual OS watch path and canonical match path for a request.
///
/// Missing targets are watched non-recursively through the nearest existing
/// directory ancestor. As path components appear, the actual watch is moved
/// closer to the requested path so broad recursive ancestor watches are never
/// needed.
fn actual_watch_path(requested: &WatchPath) -> (WatchPath, WatchPath, bool) {
    if requested.path.exists() {
        let matched_path = requested
            .path
            .canonicalize()
            .unwrap_or_else(|_| requested.path.clone());
        let actual = requested.clone();
        let matched = WatchPath {
            path: matched_path,
            recursive: requested.recursive,
        };
        return (actual, matched, false);
    }

    let requested_parent = requested.path.parent();
    let mut ancestor = requested_parent;
    while let Some(path) = ancestor {
        if path.is_dir() {
            let actual_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let matched_path = requested
                .path
                .strip_prefix(path)
                .map(|suffix| actual_path.join(suffix))
                .unwrap_or_else(|_| requested.path.clone());
            let actual = WatchPath {
                path: path.to_path_buf(),
                recursive: false,
            };
            let matched = WatchPath {
                path: matched_path,
                recursive: requested.recursive,
            };
            return (actual, matched, true);
        }
        ancestor = path.parent();
    }

    (requested.clone(), requested.clone(), false)
}

/// Converts one raw backend event path into the subscriber-visible path.
///
/// Matching first uses the canonical path namespace reported by many OS
/// backends, then falls back to the originally requested namespace for
/// synthetic tests and backends that preserve the input spelling.
fn changed_path_for_event(
    subscriber_watch: &SubscriberWatchKey,
    subscriber_watch_state: &mut SubscriberWatchState,
    event_path: &Path,
) -> Option<PathBuf> {
    if let Some(path) = changed_path_for_matched_path(
        subscriber_watch,
        subscriber_watch_state,
        &subscriber_watch_state.matched.clone(),
        event_path,
    ) {
        return Some(path);
    }
    if subscriber_watch_state.matched.path == subscriber_watch.requested.path {
        return None;
    }
    changed_path_for_matched_path(
        subscriber_watch,
        subscriber_watch_state,
        &subscriber_watch.requested,
        event_path,
    )
}

/// Applies the watch matching rules in one path namespace and maps any emitted
/// path back into the subscriber's requested namespace.
fn changed_path_for_matched_path(
    subscriber_watch: &SubscriberWatchKey,
    subscriber_watch_state: &mut SubscriberWatchState,
    matched: &WatchPath,
    event_path: &Path,
) -> Option<PathBuf> {
    let requested = &subscriber_watch.requested;
    if event_path == matched.path {
        subscriber_watch_state.last_exists = matched.path.exists();
        return Some(requested.path.clone());
    }
    if matched.path.starts_with(event_path) {
        let now_exists = matched.path.exists();
        if subscriber_watch_state.fallback {
            let should_notify = now_exists || subscriber_watch_state.last_exists;
            subscriber_watch_state.last_exists = now_exists;
            return should_notify.then(|| requested.path.clone());
        }
        if subscriber_watch_state.actual.path != matched.path {
            let should_notify = now_exists || subscriber_watch_state.last_exists;
            subscriber_watch_state.last_exists = now_exists;
            return should_notify.then(|| requested.path.clone());
        }
        subscriber_watch_state.last_exists = now_exists;
        return Some(event_path.to_path_buf());
    }
    if !event_path.starts_with(&matched.path) {
        return None;
    }
    if !(matched.recursive || event_path.parent() == Some(matched.path.as_path())) {
        return None;
    }
    subscriber_watch_state.last_exists = matched.path.exists();
    Some(
        event_path
            .strip_prefix(&matched.path)
            .map(|suffix| requested.path.join(suffix))
            .unwrap_or_else(|_| event_path.to_path_buf()),
    )
}

#[cfg(test)]
#[path = "file_watcher_tests.rs"]
mod tests;
