//! Watches subscribed files or directories and routes coarse-grained change
//! notifications to the subscribers that own matching watched paths.

use std::collections::BTreeMap;
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
use notify::event::ModifyKind;
use notify::event::RenameMode;
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::sleep_until;
use tracing::warn;

const RAW_EVENT_BUFFER_CAPACITY: usize = 1024;
/// Raw events reconciled in one pass. Draining a burst into one blocking hop
/// and one state-lock acquisition keeps the loop ahead of backend bursts
/// instead of overflowing the queue into a rescan of every subscriber.
const RAW_EVENT_BATCH_LIMIT: usize = RAW_EVENT_BUFFER_CAPACITY;
const SUBSCRIBER_PATH_BUFFER_CAPACITY: usize = 4096;
const DEGRADED_RECONCILE_INITIAL_DELAY: Duration = Duration::from_millis(25);
const DEGRADED_RECONCILE_MAX_DELAY: Duration = Duration::from_secs(1);

/// Watchers returned by [`FileWatcher::shared`], keyed by the Tokio runtime
/// that runs their event loop.
static SHARED_WATCHERS: Mutex<Vec<(tokio::runtime::Id, std::sync::Weak<FileWatcher>)>> =
    Mutex::new(Vec::new());

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
    terminal: bool,
    pending_requests: HashSet<(SubscriberId, SubscriberWatchKey)>,
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
    /// Namespace-only guards never widen delivered content invalidations.
    namespace_guard: bool,
}

/// Mutable per-subscriber watch state.
struct SubscriberWatchState {
    /// Existing path passed to the OS watcher and used for ref-counting: the
    /// canonical spelling of `requested`, or of its nearest existing directory
    /// ancestor while `requested` is missing (see `actual_watch_path`). On
    /// Windows that spelling carries the verbatim `\\?\` prefix, so it usually
    /// differs from `requested`.
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
    closed: AtomicBool,
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
                if self.inner.closed.load(Ordering::Acquire)
                    || self.inner.sender_count.load(Ordering::Acquire) == 0
                {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl WatchSender {
    fn close(&self) {
        self.mark_rescan_required();
        self.inner.closed.store(true, Ordering::Release);
        self.inner.notify.notify_one();
    }

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
            if !insert_changed_path(&mut changed_paths, path.clone()) {
                changed_paths.clear();
                drop(changed_paths);
                self.mark_rescan_required();
                return;
            }
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
        closed: AtomicBool::new(false),
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

/// Coarsens the deepest retained paths until another distinct path fits.
///
/// Overflow is rare and already means an exact path list is too large. Keeping
/// directory-level evidence is preferable to replacing every known change with
/// a global rescan marker, because consumers can still preserve freshness for
/// unrelated paths.
fn compress_changed_paths(paths: &mut BTreeSet<PathBuf>, capacity: usize) -> bool {
    if capacity == 0 {
        return false;
    }
    if paths.len() < capacity {
        return true;
    }

    // Move paths into depth buckets once. Only the deepest bucket changes in
    // each round, and popping a component reuses the owned path allocation.
    let mut by_depth = BTreeMap::<usize, BTreeSet<PathBuf>>::new();
    let mut count = paths.len();
    for path in std::mem::take(paths) {
        by_depth
            .entry(path.components().count())
            .or_default()
            .insert(path);
    }
    while count >= capacity {
        let Some((depth, deepest)) = by_depth.pop_last() else {
            break;
        };
        count -= deepest.len();
        let mut changed = false;
        for mut path in deepest {
            let parent_depth = if path.pop() {
                changed = true;
                depth - 1
            } else {
                depth
            };
            if by_depth.entry(parent_depth).or_default().insert(path) {
                count += 1;
            }
        }
        if !changed {
            break;
        }
    }
    *paths = by_depth.into_values().flatten().collect();
    count < capacity
}

/// Adds one path to a bounded changed-path set, coarsening retained paths when
/// the set is full. Returns `false` when the path cannot be retained and the
/// owner must fall back to a rescan.
fn insert_changed_path(paths: &mut BTreeSet<PathBuf>, path: PathBuf) -> bool {
    if paths.len() >= SUBSCRIBER_PATH_BUFFER_CAPACITY
        && !paths.contains(&path)
        && !compress_changed_paths(paths, SUBSCRIBER_PATH_BUFFER_CAPACITY)
    {
        return false;
    }
    paths.insert(path);
    true
}

/// Changes a receiver wrapper has accumulated but not yet emitted.
#[derive(Default)]
struct PendingChanges {
    paths: BTreeSet<PathBuf>,
    rescan_required: bool,
}

impl PendingChanges {
    fn merge(&mut self, event: FileWatcherEvent) {
        self.rescan_required |= event.rescan_required;
        if self.rescan_required {
            self.paths.clear();
            return;
        }
        for path in event.paths {
            if !insert_changed_path(&mut self.paths, path) {
                self.paths.clear();
                self.rescan_required = true;
                return;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.rescan_required
    }

    fn take(&mut self) -> Option<FileWatcherEvent> {
        if self.is_empty() {
            return None;
        }
        Some(FileWatcherEvent {
            paths: std::mem::take(&mut self.paths).into_iter().collect(),
            rescan_required: std::mem::take(&mut self.rescan_required),
        })
    }
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
    raw_tx: mpsc::WeakSender<notify::Result<Event>>,
    raw_overflow: Arc<AtomicBool>,
    raw_overflow_notify: Arc<Notify>,
    native_paths: HashMap<PathBuf, RecursiveMode>,
    needs_rebuild: bool,
    /// Explicit logical roots reconciled with the effective native coverage.
    watched_paths: HashMap<PathBuf, RecursiveMode>,
    /// Paths whose backend mode could not be reconciled with logical state.
    degraded_paths: HashSet<PathBuf>,
}

/// Coalesces bursts of watch notifications and emits at most once per interval.
pub struct ThrottledWatchReceiver {
    rx: Receiver,
    interval: Duration,
    next_allowed: Option<Instant>,
    pending: PendingChanges,
}

impl ThrottledWatchReceiver {
    /// Creates a throttling wrapper around a raw watcher [`Receiver`].
    pub fn new(rx: Receiver, interval: Duration) -> Self {
        Self {
            rx,
            interval,
            next_allowed: None,
            pending: PendingChanges::default(),
        }
    }

    /// Receives the next event immediately, then coalesces later events until
    /// the configured interval has elapsed since the previous emission.
    pub async fn recv(&mut self) -> Option<FileWatcherEvent> {
        self.recv_with_observer(|_| {}).await
    }

    /// Receives throttled events while exposing each raw event as soon as it is
    /// observed. This lets callers invalidate state immediately without also
    /// emitting externally visible notifications more than once per interval.
    pub async fn recv_with_observer(
        &mut self,
        mut observe: impl FnMut(&FileWatcherEvent),
    ) -> Option<FileWatcherEvent> {
        loop {
            let Some(next_allowed) = self.next_allowed else {
                let event = self.rx.recv().await?;
                observe(&event);
                self.next_allowed = Some(Instant::now() + self.interval);
                return Some(event);
            };

            tokio::select! {
                biased;
                _ = sleep_until(next_allowed) => {
                    self.next_allowed = None;
                    if let Some(event) = self.pending.take() {
                        self.next_allowed = Some(Instant::now() + self.interval);
                        return Some(event);
                    }
                },
                event = self.rx.recv() => match event {
                    Some(event) => {
                        observe(&event);
                        self.pending.merge(event);
                    }
                    None => return self.pending.take(),
                },
            }
        }
    }
}

/// Coalesces file watcher notifications that arrive within a fixed debounce
/// window after the first event in each batch.
pub struct DebouncedWatchReceiver {
    rx: Receiver,
    interval: Duration,
    pending: PendingChanges,
    deadline: Option<Instant>,
}

impl DebouncedWatchReceiver {
    /// Creates a debouncing wrapper around a raw watcher [`Receiver`].
    pub fn new(rx: Receiver, interval: Duration) -> Self {
        Self {
            rx,
            interval,
            pending: PendingChanges::default(),
            deadline: None,
        }
    }

    /// Receives the next debounced event batch.
    pub async fn recv(&mut self) -> Option<FileWatcherEvent> {
        while self.pending.is_empty() {
            let event = self.rx.recv().await?;
            self.pending.merge(event);
        }
        // Retain the first-event boundary when a caller cancels and recreates
        // recv (for example in select!), just as we retain its pending paths.
        let deadline = *self.deadline.get_or_insert(Instant::now() + self.interval);

        loop {
            tokio::select! {
                event = self.rx.recv() => match event {
                    Some(event) => self.pending.merge(event),
                    None => break,
                },
                _ = sleep_until(deadline) => break,
            }
        }

        // Merging never empties pending changes, so this batch is non-empty.
        self.deadline = None;
        self.pending.take()
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
        let requested_paths = dedupe_watched_paths(watched_paths);
        // Shared, nonrecursive guards cover each relocatable namespace component.
        // Include both spellings: an alias and its target can move independently.
        let mut guards = BTreeSet::new();
        for requested in &requested_paths {
            for root in [
                requested.path.clone(),
                canonical_watch_path(&requested.path),
            ] {
                guards.extend(root.ancestors().skip(1).map(Path::to_path_buf));
            }
        }
        let requests =
            requested_paths
                .into_iter()
                .map(|path| (path, false))
                .chain(guards.into_iter().map(|path| {
                    (
                        WatchPath {
                            path,
                            recursive: false,
                        },
                        true,
                    )
                }));
        let watched_paths = requests
            .map(|(requested, namespace_guard)| {
                let (actual, matched, fallback) = actual_watch_path(&requested);
                let key = SubscriberWatchKey {
                    requested,
                    namespace_guard,
                };
                SubscriberWatchRegistration {
                    key,
                    actual,
                    matched,
                    fallback,
                }
            })
            .collect::<Vec<_>>();
        let newly_covered_paths = self.file_watcher.register_paths(self.id, &watched_paths)?;

        Ok(WatchRegistration {
            file_watcher: Arc::downgrade(&self.file_watcher),
            subscriber_id: self.id,
            newly_covered_paths,
            watched_paths: watched_paths
                .iter()
                .map(|watch| watch.key.clone())
                .collect(),
        })
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
    newly_covered_paths: Vec<PathBuf>,
}

impl WatchRegistration {
    /// Requested roots that had no continuously registered content owner before
    /// this lease. Consumers must reconcile snapshots made before acquisition.
    pub fn newly_covered_paths(&self) -> &[PathBuf] {
        &self.newly_covered_paths
    }
}

impl Default for WatchRegistration {
    fn default() -> Self {
        Self {
            file_watcher: std::sync::Weak::new(),
            subscriber_id: 0,
            watched_paths: Vec::new(),
            newly_covered_paths: Vec::new(),
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
    reconcile_tx: Option<mpsc::Sender<()>>,
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
        let watcher = create_backend(
            raw_tx.clone(),
            Arc::clone(&raw_overflow),
            Arc::clone(&raw_overflow_notify),
        )?;
        let inner = FileWatcherInner {
            watcher,
            raw_tx: raw_tx.downgrade(),
            raw_overflow: Arc::clone(&raw_overflow),
            raw_overflow_notify: Arc::clone(&raw_overflow_notify),
            native_paths: HashMap::new(),
            needs_rebuild: false,
            watched_paths: HashMap::new(),
            degraded_paths: HashSet::new(),
        };
        let state = Arc::new(RwLock::new(WatchState::default()));
        let inner = Arc::new(Mutex::new(inner));
        let (reconcile_tx, reconcile_rx) = mpsc::channel(1);
        let file_watcher = Self {
            inner: Some(Arc::clone(&inner)),
            state,
            reconcile_tx: Some(reconcile_tx),
        };
        file_watcher.spawn_event_loop(&handle, raw_rx, raw_overflow, raw_overflow_notify);
        file_watcher.spawn_reconcile_loop(&handle, reconcile_rx, &inner);
        Ok(file_watcher)
    }

    /// Returns the live watcher shared by callers on the current Tokio
    /// runtime, creating it on first use.
    ///
    /// Subscribers stay independent, while overlapping registrations share a
    /// single backend watch and each backend event is reconciled once for all
    /// of them. The registry holds watchers weakly, so a shared watcher stops
    /// once its last user releases it.
    pub fn shared() -> notify::Result<Arc<Self>> {
        let runtime_id = Handle::try_current()
            .map_err(|err| {
                let message = format!("no Tokio runtime available for file watcher: {err}");
                notify::Error::generic(&message)
            })?
            .id();
        let mut watchers = SHARED_WATCHERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        watchers.retain(|(_, watcher)| watcher.upgrade().is_some_and(|watcher| watcher.is_live()));
        if let Some(watcher) = watchers
            .iter()
            .find(|(id, _)| *id == runtime_id)
            .and_then(|(_, watcher)| watcher.upgrade())
            .filter(|watcher| watcher.is_live())
        {
            return Ok(watcher);
        }
        let watcher = Arc::new(Self::new()?);
        watchers.push((runtime_id, Arc::downgrade(&watcher)));
        Ok(watcher)
    }

    /// Creates an inert watcher that only supports test-driven synthetic
    /// notifications.
    pub fn noop() -> Self {
        Self {
            inner: None,
            state: Arc::new(RwLock::new(WatchState::default())),
            reconcile_tx: None,
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
        if state.terminal {
            tx.close();
        }
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
    ) -> notify::Result<Vec<PathBuf>> {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal {
            return Err(notify::Error::generic("filesystem observer has terminated"));
        }
        let mut inner_guard = self.inner.as_ref().map(|inner| {
            inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        if inner_guard
            .as_ref()
            .is_some_and(|guard| guard.raw_tx.upgrade().is_none_or(|tx| tx.is_closed()))
        {
            state.terminal = true;
            for subscriber in state.subscribers.values() {
                subscriber.tx.close();
            }
            return Err(notify::Error::generic("filesystem observer has terminated"));
        }
        let mut newly_covered_paths = watched_paths
            .iter()
            .filter(|registration| {
                !registration.key.namespace_guard
                    && (inner_guard.as_ref().is_some_and(|guard| {
                        guard.needs_rebuild
                            || guard.degraded_paths.iter().any(|path| {
                                path_namespaces_overlap(path, &registration.actual.path)
                            })
                    }) || state.pending_requests.iter().any(|(_, key)| {
                        path_namespaces_overlap(
                            &key.requested.path,
                            &registration.key.requested.path,
                        )
                    }) || !state.subscribers.values().any(|subscriber| {
                        subscriber.watched_paths.iter().any(|(key, watch)| {
                            !key.namespace_guard
                                && (key.requested.path == registration.key.requested.path
                                    || (key.requested.recursive
                                        && registration
                                            .key
                                            .requested
                                            .path
                                            .starts_with(&key.requested.path)))
                                && (watch.matched == registration.matched
                                    || (watch.matched.recursive
                                        && registration
                                            .matched
                                            .path
                                            .starts_with(&watch.matched.path)))
                        })
                    }))
            })
            .map(|registration| registration.key.requested.path.clone())
            .collect::<Vec<_>>();
        let mut committed = Vec::new();

        for registration in watched_paths {
            let Some(subscriber) = state.subscribers.get(&subscriber_id) else {
                return Err(notify::Error::generic(
                    "file watcher subscriber was removed",
                ));
            };
            let previous = subscriber
                .watched_paths
                .get(&registration.key)
                .map(|watch| (watch.actual.clone(), watch.count));
            let registration = if previous
                .as_ref()
                .is_some_and(|(actual, _)| *actual != registration.actual)
            {
                let (actual, matched, fallback) = actual_watch_path(&registration.key.requested);
                SubscriberWatchRegistration {
                    key: registration.key.clone(),
                    actual,
                    matched,
                    fallback,
                }
            } else {
                registration.clone()
            };
            if let Some((old_actual, count)) = previous
                && old_actual != registration.actual
            {
                if !Self::apply_actual_watch_move(
                    &mut state,
                    &old_actual,
                    &registration.actual,
                    count,
                    self.inner.as_ref(),
                    self.reconcile_tx.as_ref(),
                    &mut inner_guard,
                ) {
                    state
                        .pending_requests
                        .insert((subscriber_id, registration.key.clone()));
                    for committed_watch in committed.iter().rev() {
                        self.unregister_path_locked(
                            &mut state,
                            subscriber_id,
                            committed_watch,
                            &mut inner_guard,
                        );
                    }
                    return Err(notify::Error::generic(
                        "failed to reconcile the requested watch namespace",
                    ));
                }
                if let Some(subscriber) = state.subscribers.get_mut(&subscriber_id) {
                    if let Some(watch) = subscriber.watched_paths.get_mut(&registration.key) {
                        watch.actual = registration.actual.clone();
                        watch.matched = registration.matched.clone();
                        watch.last_exists = registration.matched.path.exists();
                    }
                    subscriber.tx.mark_rescan_required();
                }
                if !registration.key.namespace_guard {
                    newly_covered_paths.push(registration.key.requested.path.clone());
                }
            }
            let actual = registration.actual.clone();
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
                self.request_degraded_reconciliation();
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


        // All requested namespace guards now have native coverage. Resolution
        // before that coverage existed may have missed an alias retarget, even
        // for a brand-new registration with no previous logical owner.
        if let Err(err) = self.reconcile_registered_paths_locked(
            &mut state,
            subscriber_id,
            watched_paths,
            &mut newly_covered_paths,
            &mut inner_guard,
        ) {
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

        newly_covered_paths.sort_unstable();
        newly_covered_paths.dedup();
        Ok(newly_covered_paths)
    }

    fn reconcile_registered_paths_locked<'a>(
        &'a self,
        state: &mut WatchState,
        subscriber_id: SubscriberId,
        registrations: &[SubscriberWatchRegistration],
        newly_covered_paths: &mut Vec<PathBuf>,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) -> notify::Result<()> {
        // Moving to a previously uncovered canonical namespace installs its
        // target guards too. Verify once after those moves; do not loop under
        // the state lock indefinitely if the caller's namespace keeps moving.
        let mut coverage_moved = false;
        for verify_only in [false, true] {
            for registration in registrations {
                let key = &registration.key;
                let (actual, matched, fallback) = actual_watch_path(&key.requested);
                let Some(watch) = state
                    .subscribers
                    .get(&subscriber_id)
                    .and_then(|subscriber| subscriber.watched_paths.get(key))
                else {
                    return Err(notify::Error::generic("file watcher subscriber was removed"));
                };
                let changed = watch.actual != actual || watch.matched != matched;
                let old_actual = watch.actual.clone();
                let count = watch.count;
                coverage_moved |= old_actual != actual;
                let reconciled = !(verify_only && changed)
                    && Self::apply_actual_watch_move(
                        state,
                        &old_actual,
                        &actual,
                        count,
                        self.inner.as_ref(),
                        self.reconcile_tx.as_ref(),
                        inner_guard,
                    );
                if !reconciled {
                    state.pending_requests.insert((subscriber_id, key.clone()));
                    if let Some(subscriber) = state.subscribers.get(&subscriber_id) {
                        subscriber.tx.mark_rescan_required();
                    }
                    self.request_degraded_reconciliation();
                    return Err(notify::Error::generic(
                        "failed to reconcile the requested watch namespace",
                    ));
                }
                if let Some(subscriber) = state.subscribers.get_mut(&subscriber_id) {
                    if let Some(watch) = subscriber.watched_paths.get_mut(key) {
                        watch.actual = actual;
                        if changed {
                            watch.last_exists = matched.path.exists();
                        }
                        watch.matched = matched;
                        watch.fallback |= fallback;
                    }
                    if changed && !key.namespace_guard {
                        // Existing owners may have observed the old target.
                        // The acquiring consumer must reconcile its snapshot as
                        // well, even if the pre-coverage resolution was shared.
                        newly_covered_paths.push(key.requested.path.clone());
                        subscriber.tx.mark_rescan_required();
                    }
                }
                state.pending_requests.remove(&(subscriber_id, key.clone()));
            }
            if !coverage_moved {
                break;
            }
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
        if state
            .subscribers
            .get(&subscriber_id)
            .is_none_or(|subscriber| !subscriber.watched_paths.contains_key(subscriber_watch))
        {
            state
                .pending_requests
                .remove(&(subscriber_id, subscriber_watch.clone()));
        }
        self.decrement_actual_watch_locked(state, &actual, 1, inner_guard);
    }

    fn remove_subscriber(&self, subscriber_id: SubscriberId) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending_requests
            .retain(|(id, _)| *id != subscriber_id);
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
            self.request_degraded_reconciliation();
        } else {
            Self::set_watch_degraded(&actual.path, next_mode, inner_guard);
        }
    }

    fn request_degraded_reconciliation(&self) {
        if let Some(reconcile_tx) = &self.reconcile_tx {
            request_reconciliation(reconcile_tx);
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
        if existing_mode == next_mode
            && !guard.needs_rebuild
            && !guard.degraded_paths.contains(path)
        {
            return Ok(());
        }
        if next_mode.is_some() && !path.exists() {
            let message = format!("watch path no longer exists: {}", path.display());
            return Err(notify::Error::generic(&message));
        }

        let mut desired = guard.watched_paths.clone();
        if let Some(mode) = next_mode {
            desired.insert(path.to_path_buf(), mode);
        } else {
            desired.remove(path);
        }
        let effective = effective_native_paths(&desired);
        let retires_coverage = guard
            .native_paths
            .iter()
            .any(|(path, mode)| effective.get(path) != Some(mode));
        // The injected failure models stopping a native root that leaves the
        // effective set. A mode change stages the root's replacement instead.
        if guard.needs_rebuild || retires_coverage {
            #[cfg(windows)]
            let retired_in_place = next_mode.is_none()
                && Self::retire_disjoint_native_paths(guard, &effective)?;
            #[cfg(not(windows))]
            let retired_in_place = false;
            if !retired_in_place {
                Self::rebuild_backend(guard, &effective)?;
            }
        } else {
            for (root, mode) in namespace_ordered_native_paths(&effective) {
                if guard.native_paths.get(root) != Some(&mode) {
                    if let Err(err) = Self::watch_backend(guard, root, mode) {
                        // A failed recursive add may already have installed descendants.
                        // Replace that uncertain instance, never accept its idempotent fast path.
                        guard.needs_rebuild = true;
                        let previous = effective_native_paths(&guard.watched_paths);
                        if let Err(cleanup) = Self::rebuild_backend(guard, &previous) {
                            warn!("failed to clean partial native registration: {cleanup}");
                        }
                        return Err(err);
                    }
                    guard.native_paths.insert(root.clone(), mode);
                }
            }
        }
        guard.watched_paths = desired;
        guard.degraded_paths.remove(path);
        Ok(())
    }

    /// The vendored Windows backend removes one exact path and acknowledges only
    /// after its callback owner stops. Pure disjoint retirement can therefore
    /// leave unrelated native registrations untouched. Other backends retain
    /// replacement semantics until their subtree-unwatch behavior is established.
    #[cfg(windows)]
    fn retire_disjoint_native_paths(
        guard: &mut FileWatcherInner,
        remaining: &HashMap<PathBuf, RecursiveMode>,
    ) -> notify::Result<bool> {
        if guard.needs_rebuild || !guard.degraded_paths.is_empty()
            || remaining.iter().any(|(root, mode)| guard.native_paths.get(root) != Some(mode))
        {
            return Ok(false);
        }
        let retired = guard.native_paths.keys()
            .filter(|root| !remaining.contains_key(*root))
            .cloned().collect::<Vec<_>>();
        if retired.is_empty() || retired.iter().any(|retired| {
            remaining.iter().any(|(root, mode)| {
                root.starts_with(retired)
                    || (*mode == RecursiveMode::Recursive && retired.starts_with(root))
            })
        }) {
            return Ok(false);
        }
        for root in retired {
            if let Err(error) = guard.watcher.unwatch(&root) {
                // Some exact removals may already have completed. Restore the
                // previous committed coverage before reporting the failed retire;
                // a failed restore keeps the existing degraded retry obligation.
                guard.needs_rebuild = true;
                let previous = effective_native_paths(&guard.watched_paths);
                if let Err(cleanup) = Self::rebuild_backend(guard, &previous) {
                    warn!("failed to restore partially retired native registration: {cleanup}");
                }
                return Err(error);
            }
        }
        guard.native_paths = remaining.clone();
        Ok(true)
    }

    /// Stage complete coverage on one replacement instance before retiring the old
    /// instance. This bypasses notify's same-mode no-op and subtree unwatch hazards.
    fn rebuild_backend(
        guard: &mut FileWatcherInner,
        roots: &HashMap<PathBuf, RecursiveMode>,
    ) -> notify::Result<()> {
        let tx = guard
            .raw_tx
            .upgrade()
            .ok_or_else(|| notify::Error::generic("filesystem observer has terminated"))?;
        let mut watcher = create_backend(
            tx,
            Arc::clone(&guard.raw_overflow),
            Arc::clone(&guard.raw_overflow_notify),
        )?;
        for (root, mode) in namespace_ordered_native_paths(roots) {
            watcher.watch(root, mode)?;
        }
        guard.watcher = watcher;
        guard.native_paths = roots.clone();
        guard.needs_rebuild = false;
        Ok(())
    }

    pub fn is_live(&self) -> bool {
        !self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal
            && self.inner.as_ref().is_none_or(|inner| {
                inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .raw_tx
                    .upgrade()
                    .is_some_and(|tx| !tx.is_closed())
            })
    }

    fn watch_backend(
        guard: &mut FileWatcherInner,
        path: &Path,
        mode: RecursiveMode,
    ) -> notify::Result<()> {
        guard.watcher.watch(path, mode)
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

    fn spawn_reconcile_loop(
        &self,
        handle: &Handle,
        mut reconcile_rx: mpsc::Receiver<()>,
        inner: &Arc<Mutex<FileWatcherInner>>,
    ) {
        let state = Arc::clone(&self.state);
        let inner = Arc::downgrade(inner);
        handle.spawn(async move {
            while reconcile_rx.recv().await.is_some() {
                let mut retry_delay = DEGRADED_RECONCILE_INITIAL_DELAY;
                loop {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    let reconcile_state = Arc::clone(&state);
                    let retry = tokio::task::spawn_blocking(move || {
                        Self::reconcile_degraded_paths(&reconcile_state, &inner)
                    })
                    .await;
                    match retry {
                        Ok(false) => break,
                        Ok(true) => {}
                        Err(err) => {
                            warn!("file watcher reconciliation worker failed: {err}");
                            break;
                        }
                    }

                    sleep(retry_delay).await;
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(DEGRADED_RECONCILE_MAX_DELAY);
                    // Capacity is one; a live producer can refill it indefinitely.
                    let _ = reconcile_rx.try_recv();
                }
            }
        });
    }

    fn reconcile_degraded_paths(
        state: &RwLock<WatchState>,
        inner: &Arc<Mutex<FileWatcherInner>>,
    ) -> bool {
        let pending = {
            let state = state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.terminal {
                return false;
            }
            state
                .pending_requests
                .iter()
                .map(|(_, key)| key.requested.path.clone())
                .collect::<Vec<_>>()
        };
        if !pending.is_empty() {
            let mut changes = ObservedChanges {
                paths: pending,
                ..Default::default()
            };
            changes.normalize();
            for (subscriber, _) in Self::reconcile_subscribers(state, Some(inner), None, &changes) {
                // Coverage is restored before the refresh hint. Retry only
                // pending namespaces, not every unrelated recursive tree.
                subscriber.mark_rescan_required();
            }
        }
        let state = state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut inner_guard = Some(
            inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let degraded_paths = inner_guard
            .as_ref()
            .map(|guard| guard.degraded_paths.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        for path in degraded_paths {
            let desired_mode = state
                .path_ref_counts
                .get(&path)
                .copied()
                .and_then(PathWatchCounts::effective_mode);
            if let Err(err) =
                Self::reconfigure_watch_inner(Some(inner), &path, desired_mode, &mut inner_guard)
            {
                warn!(
                    "failed to reconcile degraded file watch {}: {err}",
                    path.display()
                );
                Self::mark_watch_degraded(&path, &mut inner_guard);
            } else {
                Self::set_watch_degraded(&path, desired_mode, &mut inner_guard);
                if desired_mode.is_some() {
                    // Changes made while the backend watch was missing produced
                    // no events, and the rescan signalled at degradation may
                    // already have been consumed.
                    Self::mark_subscribers_rescan_for_path(&state, &path);
                }
            }
        }

        inner_guard
            .as_ref()
            .is_some_and(|guard| !guard.degraded_paths.is_empty())
            || !state.pending_requests.is_empty()
    }

    fn apply_actual_watch_move<'a>(
        state: &mut WatchState,
        old_actual: &WatchPath,
        new_actual: &WatchPath,
        count: usize,
        inner: Option<&'a Arc<Mutex<FileWatcherInner>>>,
        reconcile_tx: Option<&mpsc::Sender<()>>,
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
                if let Some(reconcile_tx) = reconcile_tx {
                    request_reconciliation(reconcile_tx);
                }
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
            if let Some(reconcile_tx) = reconcile_tx {
                request_reconciliation(reconcile_tx);
            }
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
            if let Some(reconcile_tx) = reconcile_tx {
                request_reconciliation(reconcile_tx);
            }
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
        let reconcile_tx = self.reconcile_tx.clone();
        handle.spawn(async move {
            loop {
                let mut changes = ObservedChanges::default();
                let closed = tokio::select! {
                    biased;
                    raw_event = raw_rx.recv() => match raw_event {
                        Some(raw_event) => {
                            changes.record(raw_event);
                            for _ in 1..RAW_EVENT_BATCH_LIMIT {
                                let Ok(raw_event) = raw_rx.try_recv() else {
                                    break;
                                };
                                changes.record(raw_event);
                            }
                            false
                        }
                        None => true,
                    },
                    _ = raw_overflow_notify.notified() => false,
                };
                changes.rescan_required |= raw_overflow.swap(false, Ordering::AcqRel);
                if !changes.is_empty() {
                    let inner = inner.as_ref().and_then(std::sync::Weak::upgrade);
                    Self::reconcile_changes(&state, inner.as_ref(), reconcile_tx.as_ref(), changes)
                        .await;
                }
                if closed {
                    let mut state = state
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.terminal = true;
                    state.pending_requests.clear();
                    for subscriber in state.subscribers.values() {
                        subscriber.tx.close();
                    }
                    break;
                }
            }
        });
    }

    async fn reconcile_changes(
        state: &Arc<RwLock<WatchState>>,
        inner: Option<&Arc<Mutex<FileWatcherInner>>>,
        reconcile_tx: Option<&mpsc::Sender<()>>,
        mut changes: ObservedChanges,
    ) {
        changes.normalize();
        let state = Arc::clone(state);
        let inner = inner.cloned();
        let reconcile_tx = reconcile_tx.cloned();
        let subscribers_to_notify = tokio::task::spawn_blocking(move || {
            Self::reconcile_subscribers(&state, inner.as_ref(), reconcile_tx.as_ref(), &changes)
        })
        .await;
        match subscribers_to_notify {
            Ok(subscribers) => {
                for (subscriber, changed_paths) in subscribers {
                    subscriber.add_changed_paths(&changed_paths).await;
                }
            }
            Err(err) => warn!("file watcher event reconciliation worker failed: {err}"),
        }
    }

    fn reconcile_subscribers(
        state: &RwLock<WatchState>,
        inner: Option<&Arc<Mutex<FileWatcherInner>>>,
        reconcile_tx: Option<&mpsc::Sender<()>>,
        changes: &ObservedChanges,
    ) -> Vec<(WatchSender, Vec<PathBuf>)> {
        let mut state = state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rescan = changes.rescan_required;
        let mut actual_watch_moves = Vec::new();
        let mut subscribers_to_notify = Vec::new();
        let mut relevant_paths = Vec::new();
        // Subscribers sharing a requested path can share its resolution within
        // this pass. Never retain filesystem observations across event batches.
        let mut resolved_paths = HashMap::new();

        let WatchState {
            subscribers,
            path_ref_counts,
            pending_requests,
            ..
        } = &mut *state;
        for (subscriber_id, subscriber) in subscribers {
            let mut changed_paths = BTreeSet::new();
            let mut rescan_required = false;
            let guard_mapped_paths = if rescan {
                HashMap::new()
            } else {
                guard_mapped_event_paths(&subscriber.watched_paths, &changes.paths)
            };
            let mut requested_namespace_paths = Vec::new();
            for (subscriber_watch, subscriber_watch_state) in &mut subscriber.watched_paths {
                relevant_paths.clear();
                requested_namespace_paths.clear();
                if !rescan {
                    relevant_paths.extend(changes.paths.iter().filter(|event_path| {
                        event_may_affect_watch(subscriber_watch, subscriber_watch_state, event_path)
                            // A shared ancestor watch can report the directory's
                            // own timestamp on Windows. A stable watch rooted
                            // there watches its contents, not that timestamp.
                            // Keep structural events and all descendant events.
                            && !(changes.root_metadata_paths.contains(*event_path)
                                && subscriber_watch_is_stable(subscriber_watch, subscriber_watch_state)
                                && (**event_path == subscriber_watch_state.matched.path
                                    || **event_path == subscriber_watch.requested.path)
                                && event_path.is_dir())
                    }));
                    // Reports only a namespace guard places, such as the entry
                    // of an alias on the requested path. As below, a timestamp
                    // report for a watched directory above a stable watch is
                    // left out: the entry leading to the watch reports itself.
                    requested_namespace_paths.extend(
                        subscriber_watch
                            .requested
                            .path
                            .ancestors()
                            .filter_map(|requested_path| {
                                guard_mapped_paths.get_key_value(requested_path)
                            })
                            .filter(|(requested_path, event_paths)| {
                                event_paths.iter().any(|event_path| {
                                    !event_may_affect_watch(
                                        subscriber_watch,
                                        subscriber_watch_state,
                                        event_path,
                                    ) && !(changes.root_metadata_paths.contains(*event_path)
                                        && subscriber_watch_is_stable(
                                            subscriber_watch,
                                            subscriber_watch_state,
                                        )
                                        && **requested_path != subscriber_watch.requested.path
                                        && path_ref_counts.contains_key(*event_path)
                                        && event_path.is_dir())
                                })
                            })
                            .map(|(requested_path, _)| requested_path),
                    );
                    if relevant_paths.is_empty() && requested_namespace_paths.is_empty() {
                        continue;
                    }
                }
                // Lost events can hide a removed or replaced root, so a rescan
                // re-resolves every watch rather than only fallback watches.
                let stable_descendant_event = !rescan
                    && requested_namespace_paths.is_empty()
                    && subscriber_watch_is_stable(subscriber_watch, subscriber_watch_state)
                    && relevant_paths.iter().all(|event_path| {
                        is_strict_descendant(event_path, &subscriber_watch_state.matched.path)
                            || is_strict_descendant(event_path, &subscriber_watch.requested.path)
                    });
                let (new_actual, new_matched, fallback) = if stable_descendant_event {
                    (
                        subscriber_watch_state.actual.clone(),
                        subscriber_watch_state.matched.clone(),
                        false,
                    )
                } else {
                    resolved_paths
                        .entry(subscriber_watch.requested.clone())
                        .or_insert_with(|| {
                            actual_watch_path(&subscriber_watch.requested)
                        })
                        .clone()
                };
                // A Windows timestamp report for a directory above a stable
                // watch says only that the directory's entries changed. While
                // that directory is itself watched, the entry leading to this
                // watch reports its own creation, removal, or rename, and the
                // resolution above has already caught a retargeted link.
                let unmoved_stable_watch =
                    subscriber_watch_is_stable(subscriber_watch, subscriber_watch_state)
                        && subscriber_watch_state.actual == new_actual
                        && subscriber_watch_state.matched == new_matched;
                for event_path in relevant_paths
                    .iter()
                    .filter(|_| !subscriber_watch.namespace_guard)
                {
                    if unmoved_stable_watch
                        && changes.root_metadata_paths.contains(*event_path)
                        && path_ref_counts.contains_key(*event_path)
                        && (is_strict_descendant(&subscriber_watch_state.matched.path, event_path)
                            || is_strict_descendant(&subscriber_watch.requested.path, event_path))
                        && event_path.is_dir()
                    {
                        continue;
                    }
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
                        && !insert_changed_path(&mut changed_paths, path)
                    {
                        changed_paths.clear();
                        rescan_required = true;
                    }
                }
                // A guard placed these reports at or above the requested path.
                for requested_path in requested_namespace_paths
                    .iter()
                    .filter(|_| !subscriber_watch.namespace_guard)
                {
                    let changed_path = changed_path_for_matched_path(
                        subscriber_watch,
                        subscriber_watch_state,
                        &subscriber_watch.requested,
                        requested_path,
                    );
                    if let Some(path) = changed_path
                        && !rescan_required
                        && !insert_changed_path(&mut changed_paths, path)
                    {
                        changed_paths.clear();
                        rescan_required = true;
                    }
                }

                subscriber_watch_state.fallback |= fallback;
                if subscriber_watch_state.actual == new_actual {
                    // Re-resolution may find the target absent again. Its
                    // existing fallback/guards now own future promotion.
                    pending_requests.remove(&(*subscriber_id, subscriber_watch.clone()));
                    if rescan || subscriber_watch_state.matched != new_matched {
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
                reconcile_tx,
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
                state
                    .pending_requests
                    .remove(&(subscriber_id, subscriber_watch));
            } else {
                subscriber.tx.mark_rescan_required();
                state
                    .pending_requests
                    .insert((subscriber_id, subscriber_watch));
            }
        }

        // Overflow can discard the removal report for a root recreated before
        // this pass. Its logical path is unchanged, but its backend watch may
        // be gone. Re-arm each shared actual watch once before publishing rescan.
        let roots_to_rearm = if rescan {
            state.path_ref_counts.keys().cloned().collect::<Vec<_>>()
        } else {
            changes.lost_watch_roots.clone()
        };
        for root in &roots_to_rearm {
            // The backend drops a watch whose root is removed. When the root is
            // back before this pass, resolution keeps the watch where it was,
            // so re-arm it before subscribers learn that the root changed.
            let Some(mode) = state
                .path_ref_counts
                .get(root)
                .copied()
                .and_then(PathWatchCounts::effective_mode)
            else {
                continue;
            };
            if let Err(err) = Self::rewatch_backend(inner, root, mode, &mut inner_guard) {
                warn!(
                    "failed to re-establish file watch {}: {err}",
                    root.display()
                );
                // The failed replacement was this batch's only re-arm attempt,
                // so every referenced lost root stays degraded until reconciled;
                // recovery then requests a rescan for each of them.
                for lost_root in &roots_to_rearm {
                    if state
                        .path_ref_counts
                        .get(lost_root)
                        .copied()
                        .and_then(PathWatchCounts::effective_mode)
                        .is_some()
                    {
                        Self::mark_watch_degraded(lost_root, &mut inner_guard);
                        Self::mark_subscribers_rescan_for_path(&state, lost_root);
                    }
                }
                if let Some(reconcile_tx) = reconcile_tx {
                    request_reconciliation(reconcile_tx);
                }
            }
            // A replacement rebuilds all effective roots once per loss batch.
            break;
        }

        if rescan {
            // Only after every watch is re-resolved and re-armed: a subscriber
            // must not finish its rescan before the watches it relies on exist.
            for subscriber in state.subscribers.values() {
                subscriber.tx.mark_rescan_required();
            }
        }
        subscribers_to_notify
    }

    /// Re-arms a backend watch the backend may have dropped. Watching a path
    /// replaces a watch the backend still holds, so a surviving watch never
    /// loses coverage.
    fn rewatch_backend<'a>(
        inner: Option<&'a Arc<Mutex<FileWatcherInner>>>,
        path: &Path,
        mode: RecursiveMode,
        inner_guard: &mut Option<std::sync::MutexGuard<'a, FileWatcherInner>>,
    ) -> notify::Result<()> {
        let Some(inner) = inner else {
            return Ok(());
        };
        let guard = inner_guard.get_or_insert_with(|| {
            inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let _ = (path, mode);
        guard.needs_rebuild = true;
        let roots = effective_native_paths(&guard.watched_paths);
        Self::rebuild_backend(guard, &roots)?;
        // Re-arming installed coverage does not reconcile logical ownership.
        // Preserve failed-unregistration obligations for the retry worker.
        Ok(())
    }




}

fn request_reconciliation(reconcile_tx: &mpsc::Sender<()>) {
    let _ = reconcile_tx.try_send(());
}

fn create_backend(
    raw_tx: mpsc::Sender<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
    wake: Arc<Notify>,
) -> notify::Result<RecommendedWatcher> {
    RecommendedWatcher::new(
        move |event| {
            enqueue_raw_event(&raw_tx, &overflow, &wake, event);
        },
        notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
    )
}

/// Native recursive coverage subsumes child registrations, not their delivery scope.
fn effective_native_paths(
    paths: &HashMap<PathBuf, RecursiveMode>,
) -> HashMap<PathBuf, RecursiveMode> {
    let mut paths = paths.clone();
    // Derived target guards move with canonical content roots, including after
    // an alias retarget. Explicit requested guards protect the alias namespace.
    let parents = paths
        .keys()
        .flat_map(|path| path.ancestors().skip(1).map(Path::to_path_buf))
        .collect::<HashSet<_>>();
    for parent in parents {
        if parent.is_dir() {
            paths.entry(parent).or_insert(RecursiveMode::NonRecursive);
        }
    }
    paths
        .iter()
        .filter(|(path, _)| {
            !paths.iter().any(|(ancestor, mode)| {
                **path != *ancestor
                    && path.starts_with(ancestor)
                    && *mode == RecursiveMode::Recursive
            })
        })
        .map(|(path, mode)| (path.clone(), *mode))
        .collect()
}

/// Cover a root's namespace before attaching to its current object. A removal
/// and recreation during attachment must be observable even at the same path.
fn namespace_ordered_native_paths(
    paths: &HashMap<PathBuf, RecursiveMode>,
) -> Vec<(&PathBuf, RecursiveMode)> {
    let mut paths = paths
        .iter()
        .map(|(path, mode)| (path, *mode))
        .collect::<Vec<_>>();
    paths.sort_unstable_by_key(|(path, _)| path.components().count());
    paths
}

fn canonical_watch_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn enqueue_raw_event(
    raw_tx: &mpsc::Sender<notify::Result<Event>>,
    raw_overflow: &AtomicBool,
    raw_overflow_notify: &Notify,
    event: notify::Result<Event>,
) {
    if event
        .as_ref()
        .is_ok_and(|event| !event.need_rescan() && !is_mutating_event(event))
    {
        return;
    }
    // A backend rescan event (for example a Windows ReadDirectoryChangesW
    // overflow) means changes were discarded without identifying their paths.
    if event.as_ref().is_ok_and(|event| {
        event.need_rescan() || event.paths.len() > SUBSCRIBER_PATH_BUFFER_CAPACITY
    }) {
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

/// Returns whether the object previously at each event path may be gone. A
/// backend watch rooted at such a path can be dropped: notify's Windows backend
/// unwatches a watched directory once it reports that directory's removal.
fn event_may_drop_watch_root(event: &Event) -> bool {
    match event.kind {
        EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Name(mode)) => mode != RenameMode::To,
        _ => false,
    }
}

/// Backend observations reconciled in one pass.
#[derive(Default)]
struct ObservedChanges {
    /// Changed paths reported by the backend.
    paths: Vec<PathBuf>,
    /// Windows directory metadata reports from a shared ancestor watch must
    /// not broaden a stable directory-rooted subscription to its own metadata.
    root_metadata_paths: HashSet<PathBuf>,
    structural_paths: HashSet<PathBuf>,
    /// Paths whose previous object was removed or renamed away, or that a
    /// backend error named; a backend watch rooted there may be gone.
    lost_watch_roots: Vec<PathBuf>,
    /// Whether changes were lost, so every watch is re-resolved and every
    /// subscriber must rescan instead of relying on `paths`.
    rescan_required: bool,
}

impl ObservedChanges {
    fn record(&mut self, raw_event: notify::Result<Event>) {
        match raw_event {
            Ok(event) => {
                if !is_mutating_event(&event) || event.paths.is_empty() {
                    return;
                }
                if cfg!(windows) && event.kind == EventKind::Modify(ModifyKind::Any) {
                    self.root_metadata_paths.extend(event.paths.iter().cloned());
                } else {
                    self.structural_paths.extend(event.paths.iter().cloned());
                }
                if event_may_drop_watch_root(&event) {
                    self.lost_watch_roots.extend(event.paths.iter().cloned());
                }
                self.paths.extend(event.paths);
            }
            Err(err) => {
                warn!("file watcher error requiring rescan: {err}");
                // An error that stops a backend watch names that watch's root.
                self.lost_watch_roots.extend(err.paths);
                self.rescan_required = true;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.lost_watch_roots.is_empty() && !self.rescan_required
    }

    fn normalize(&mut self) {
        self.root_metadata_paths
            .retain(|path| !self.structural_paths.contains(path));
        self.paths.sort_unstable();
        self.paths.dedup();
        self.lost_watch_roots.sort_unstable();
        self.lost_watch_roots.dedup();
    }
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
        let matched = WatchPath {
            path: matched_path,
            recursive: requested.recursive,
        };
        return (matched.clone(), matched, false);
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
                path: actual_path,
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

fn path_namespaces_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn is_strict_descendant(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

/// Returns whether a backend event can change one subscriber watch.
///
/// Every watch cares about its requested and canonical namespaces. A fallback
/// watch also re-resolves for events its non-recursive ancestor watch reports:
/// the ancestor or a direct child, where a missing component or symlink target
/// can appear. `actual` is already that ancestor's canonical location, the
/// spelling the backend reports, so those checks cover it directly.
/// Unrelated events elsewhere in the watcher must not cost every fallback
/// watch a filesystem resolution under the state lock. Reports that only a
/// namespace guard can place are matched separately; see
/// `guard_mapped_event_paths`.
fn event_may_affect_watch(
    subscriber_watch: &SubscriberWatchKey,
    subscriber_watch_state: &SubscriberWatchState,
    event_path: &Path,
) -> bool {
    if path_namespaces_overlap(event_path, &subscriber_watch.requested.path)
        || path_namespaces_overlap(event_path, &subscriber_watch_state.matched.path)
    {
        return true;
    }
    if !subscriber_watch_state.fallback {
        return false;
    }
    let actual = subscriber_watch_state.actual.path.as_path();
    event_path == actual || event_path.parent() == Some(actual)
}

/// Maps backend reports into requested spellings through namespace guards.
///
/// A native watch reports an entry in its directory's canonical spelling. When
/// a request differs from that spelling above an alias on its path (a Windows
/// path without `\\?\`, macOS `/var` for `/private/var`, or an alias inside an
/// aliased directory), the report of that alias entry, such as a retarget,
/// matches neither the requested namespace nor the canonical one, which is the
/// alias's target. A guard pairs a requested directory with its canonical
/// location, so it maps a report of that directory or of one of its entries
/// back to the requested spelling. The result maps each requested-spelling
/// path to the reports that name it.
fn guard_mapped_event_paths<'a>(
    watched_paths: &HashMap<SubscriberWatchKey, SubscriberWatchState>,
    event_paths: &'a [PathBuf],
) -> HashMap<PathBuf, Vec<&'a Path>> {
    let mut guards = HashMap::<&Path, Vec<&Path>>::new();
    for (subscriber_watch, subscriber_watch_state) in watched_paths {
        if subscriber_watch.namespace_guard
            && subscriber_watch.requested.path != subscriber_watch_state.matched.path
        {
            guards
                .entry(subscriber_watch_state.matched.path.as_path())
                .or_default()
                .push(subscriber_watch.requested.path.as_path());
        }
    }
    let mut mapped = HashMap::<PathBuf, Vec<&Path>>::new();
    if guards.is_empty() {
        return mapped;
    }
    for event_path in event_paths {
        if let Some(requested_dirs) = guards.get(event_path.as_path()) {
            for requested_dir in requested_dirs {
                mapped
                    .entry(requested_dir.to_path_buf())
                    .or_default()
                    .push(event_path.as_path());
            }
        }
        if let Some(name) = event_path.file_name()
            && let Some(requested_dirs) = event_path.parent().and_then(|parent| guards.get(parent))
        {
            for requested_dir in requested_dirs {
                mapped
                    .entry(requested_dir.join(name))
                    .or_default()
                    .push(event_path.as_path());
            }
        }
    }
    mapped
}

fn subscriber_watch_is_stable(
    _subscriber_watch: &SubscriberWatchKey,
    subscriber_watch_state: &SubscriberWatchState,
) -> bool {
    subscriber_watch_state.last_exists
        && subscriber_watch_state.actual == subscriber_watch_state.matched
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
        if subscriber_watch_state.actual.path != matched.path || requested.path != matched.path {
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
    subscriber_watch_state.last_exists =
        if subscriber_watch_is_stable(subscriber_watch, subscriber_watch_state) {
            true
        } else {
            matched.path.exists()
        };
    Some(
        event_path
            .strip_prefix(&matched.path)
            .map(|suffix| requested.path.join(suffix))
            .unwrap_or_else(|_| event_path.to_path_buf()),
    )
}

