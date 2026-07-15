use std::borrow::Borrow;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex as StdMutex;

use lru::LruCache;
use sha1::Digest;
use sha1::Sha1;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;

/// A minimal LRU cache protected by a Tokio mutex.
/// Calls outside a Tokio runtime are no-ops.
pub struct BlockingLruCache<K, V> {
    inner: Mutex<LruCache<K, V>>,
    flights: StdMutex<Vec<FlightEntry<K, V>>>,
}

struct FlightEntry<K, V> {
    key: K,
    state: Arc<FlightState<V>>,
}

struct FlightState<V> {
    state: StdMutex<FlightResult<V>>,
    wake: Condvar,
}

struct FlightResult<V> {
    completed: bool,
    invalidated: bool,
    waiters: usize,
    winner: Option<V>,
}

impl<V> Default for FlightState<V> {
    fn default() -> Self {
        Self {
            state: StdMutex::new(FlightResult {
                completed: false,
                invalidated: false,
                waiters: 0,
                winner: None,
            }),
            wake: Condvar::new(),
        }
    }
}

impl<V> FlightState<V>
where
    V: Clone,
{
    fn subscribe(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiters = state.waiters.saturating_add(1);
    }

    fn wait(&self) -> Option<V> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.completed {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.waiters = state.waiters.saturating_sub(1);
        let winner = state.winner.clone();
        if state.waiters == 0 {
            state.winner = None;
        }
        winner
    }

    fn complete_success(&self, value: &V) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.completed {
            if state.waiters > 0 {
                state.winner = Some(value.clone());
            }
            state.completed = true;
            self.wake.notify_all();
        }
    }

    fn complete_retry(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.completed {
            state.completed = true;
            self.wake.notify_all();
        }
    }
}

impl<V> FlightState<V> {
    fn invalidate(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidated = true;
    }

    fn is_invalidated(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidated
    }
}

struct FlightLeader<'a, K: Eq + Hash, V: Clone> {
    cache: &'a BlockingLruCache<K, V>,
    state: Arc<FlightState<V>>,
    active: bool,
}

impl<K, V> BlockingLruCache<K, V>
where
    K: Eq + Hash,
{
    /// Creates a cache with the provided non-zero capacity.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(capacity)),
            flights: StdMutex::new(Vec::new()),
        }
    }

    /// Returns a clone of the cached value for `key`, or computes and inserts it.
    pub fn get_or_insert_with(&self, key: K, value: impl FnOnce() -> V) -> V
    where
        V: Clone,
    {
        match self.get_or_try_insert_with(key, || Ok::<V, std::convert::Infallible>(value())) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    /// Like `get_or_insert_with`, but the value factory may fail.
    pub fn get_or_try_insert_with<E>(
        &self,
        key: K,
        value: impl FnOnce() -> Result<V, E>,
    ) -> Result<V, E>
    where
        V: Clone,
    {
        self.get_or_try_insert_with_mut(key, value, |cache, key, value| {
            cache.put(key, value.clone());
        })
    }

    /// Like [`Self::get_or_try_insert_with`], with caller-controlled insertion.
    ///
    /// The commit callback runs under the cache mutex after a successful factory. This is useful
    /// for caches that enforce constraints in addition to entry count, such as a total byte limit.
    pub fn get_or_try_insert_with_mut<E>(
        &self,
        key: K,
        value: impl FnOnce() -> Result<V, E>,
        commit: impl FnOnce(&mut LruCache<K, V>, K, &V),
    ) -> Result<V, E>
    where
        V: Clone,
    {
        if tokio::runtime::Handle::try_current().is_err() {
            return value();
        }

        let mut key = Some(key);
        let mut value = Some(value);
        let mut commit = Some(commit);

        loop {
            let mut guard = lock_if_runtime(&self.inner).expect("runtime checked above");
            let lookup_key = key.as_ref().expect("key remains owned by waiter");
            if let Some(cached) = guard.get(lookup_key) {
                return Ok(cached.clone());
            }

            let mut flights = self
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(flight) = flights
                .iter()
                .find(|entry| entry.key == *lookup_key)
                .map(|entry| Arc::clone(&entry.state))
            {
                flight.subscribe();
                drop(flights);
                drop(guard);
                if let Some(winner) = tokio::task::block_in_place(|| flight.wait()) {
                    return Ok(winner);
                }
                continue;
            }

            let state = Arc::new(FlightState::default());
            flights.push(FlightEntry {
                key: key.take().expect("leader owns key"),
                state: Arc::clone(&state),
            });
            drop(flights);
            drop(guard);

            let mut leader = FlightLeader {
                cache: self,
                state,
                active: true,
            };
            let computed = value.take().expect("factory runs once")();
            match computed {
                Ok(computed) => {
                    leader.commit(&computed, commit.take().expect("commit callback runs once"));
                    return Ok(computed);
                }
                Err(error) => {
                    leader.finish_without_commit();
                    return Err(error);
                }
            }
        }
    }

    /// Builds a cache if `capacity` is non-zero, returning `None` otherwise.
    #[must_use]
    pub fn try_with_capacity(capacity: usize) -> Option<Self> {
        NonZeroUsize::new(capacity).map(Self::new)
    }

    /// Returns a clone of the cached value corresponding to `key`, if present.
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        V: Clone,
    {
        let mut guard = lock_if_runtime(&self.inner)?;
        guard.get(key).cloned()
    }

    /// Inserts `value` for `key`, returning the previous entry if it existed.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let mut guard = lock_if_runtime(&self.inner)?;
        self.invalidate_flight_for(&key);
        guard.put(key, value)
    }

    /// Removes the entry for `key` if it exists, returning it.
    pub fn remove<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut guard = lock_if_runtime(&self.inner)?;
        self.invalidate_flight_for(key);
        guard.pop(key)
    }

    /// Clears all entries from the cache.
    pub fn clear(&self) {
        if let Some(mut guard) = lock_if_runtime(&self.inner) {
            self.invalidate_all_flights();
            guard.clear();
        }
    }

    /// Executes `callback` with a mutable reference to the underlying cache.
    pub fn with_mut<R>(&self, callback: impl FnOnce(&mut LruCache<K, V>) -> R) -> R {
        if let Some(mut guard) = lock_if_runtime(&self.inner) {
            self.invalidate_all_flights();
            callback(&mut guard)
        } else {
            let mut disabled = LruCache::unbounded();
            callback(&mut disabled)
        }
    }

    /// Provides direct access to the cache guard when a Tokio runtime is available.
    pub fn blocking_lock(&self) -> Option<MutexGuard<'_, LruCache<K, V>>> {
        let guard = lock_if_runtime(&self.inner)?;
        self.invalidate_all_flights();
        Some(guard)
    }

    fn invalidate_flight_for<Q>(&self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let flights = self
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in flights.iter().filter(|entry| entry.key.borrow() == key) {
            entry.state.invalidate();
        }
    }

    fn invalidate_all_flights(&self) {
        let flights = self
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in flights.iter() {
            entry.state.invalidate();
        }
    }
}

impl<K, V> FlightLeader<'_, K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    fn commit(&mut self, value: &V, commit: impl FnOnce(&mut LruCache<K, V>, K, &V)) {
        let mut cache = lock_if_runtime(&self.cache.inner).expect("leader requires runtime");
        // Declared after the cache guard so unwind publishes completion before releasing the cache.
        let completion = FlightCompletion(Arc::clone(&self.state));
        let entry = self.take_entry();
        if !self.state.is_invalidated() {
            commit(&mut cache, entry.key, value);
            self.state.complete_success(value);
        }
        self.active = false;
        drop(completion);
    }

    fn finish_without_commit(&mut self) {
        let _cache = lock_if_runtime(&self.cache.inner).expect("leader requires runtime");
        let _completion = FlightCompletion(Arc::clone(&self.state));
        let _ = self.take_entry();
        self.active = false;
    }

    fn take_entry(&self) -> FlightEntry<K, V> {
        let mut flights = self
            .cache
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = flights
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.state, &self.state))
            .expect("leader flight is registered");
        flights.swap_remove(index)
    }
}

impl<K, V> Drop for FlightLeader<'_, K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(_cache) = lock_if_runtime(&self.cache.inner) {
            let mut flights = self
                .cache
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(index) = flights
                .iter()
                .position(|entry| Arc::ptr_eq(&entry.state, &self.state))
            {
                flights.swap_remove(index);
            }
            self.state.complete_retry();
        }
        self.active = false;
    }
}

struct FlightCompletion<V>(Arc<FlightState<V>>);

impl<V> Drop for FlightCompletion<V> {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.completed {
            state.completed = true;
            self.0.wake.notify_all();
        }
    }
}

fn lock_if_runtime<K, V>(m: &Mutex<LruCache<K, V>>) -> Option<MutexGuard<'_, LruCache<K, V>>>
where
    K: Eq + Hash,
{
    tokio::runtime::Handle::try_current().ok()?;
    Some(tokio::task::block_in_place(|| m.blocking_lock()))
}

/// Computes the SHA-1 digest of `bytes`.
///
/// Useful for content-based cache keys when you want to avoid staleness
/// caused by path-only keys.
#[must_use]
pub fn sha1_digest(bytes: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(bytes);
    let result = hasher.finalize();
    let mut out = [0; 20];
    out.copy_from_slice(&result);
    out
}

#[cfg(test)]
mod tests {
    use super::BlockingLruCache;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        wake: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().expect("gate lock");
            while !*open {
                open = self.wake.wait(open).expect("gate wait");
            }
        }

        fn open(&self) {
            *self.open.lock().expect("gate lock") = true;
            self.wake.notify_all();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_and_retrieves_values() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));

        assert!(cache.get(&"first").is_none());
        cache.insert("first", /*value*/ 1);
        assert_eq!(cache.get(&"first"), Some(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evicts_least_recently_used() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));
        cache.insert("a", /*value*/ 1);
        cache.insert("b", /*value*/ 2);
        assert_eq!(cache.get(&"a"), Some(1));

        cache.insert("c", /*value*/ 3);

        assert!(cache.get(&"b").is_none());
        assert_eq!(cache.get(&"a"), Some(1));
        assert_eq!(cache.get(&"c"), Some(3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_same_key_runs_one_factory() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(tokio::sync::Barrier::new(9));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            let start = Arc::clone(&start);
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                cache.get_or_insert_with("shared", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    7
                })
            }));
        }
        start.wait().await;

        for task in tasks {
            assert_eq!(task.await.expect("task"), 7);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn waiters_share_success_when_commit_declines_insertion() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(tokio::sync::Barrier::new(9));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let calls = Arc::clone(&calls);
            let start = Arc::clone(&start);
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                cache.get_or_try_insert_with_mut(
                    "shared",
                    || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        Ok::<_, std::convert::Infallible>(7)
                    },
                    |_cache, _key, _value| {},
                )
            }));
        }
        start.wait().await;

        for task in tasks {
            assert_eq!(task.await.expect("task"), Ok(7));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(cache.get(&"shared").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_does_not_wait_for_an_active_factory() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_insert_with("key", || {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    7
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");

        let (lookup_tx, lookup_rx) = std::sync::mpsc::channel();
        let lookup = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                let result = cache.get(&"key");
                lookup_tx.send(result).expect("lookup result");
            })
        };
        let lookup_result = lookup_rx.recv_timeout(Duration::from_millis(100));
        gate.open();
        assert_eq!(leader.await.expect("leader task"), 7);
        lookup.await.expect("lookup task");
        assert_eq!(lookup_result, Ok(None));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_keys_compute_concurrently_and_commit_in_completion_order() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(1).expect("capacity"),
        ));
        let slow_gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();

        let slow = {
            let cache = Arc::clone(&cache);
            let slow_gate = Arc::clone(&slow_gate);
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                cache.get_or_insert_with("slow", || {
                    started_tx.send("slow").expect("started");
                    slow_gate.wait();
                    1
                })
            })
        };
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(1)), Ok("slow"));

        let fast = {
            let cache = Arc::clone(&cache);
            let started_tx = started_tx.clone();
            tokio::spawn(async move {
                cache.get_or_insert_with("fast", || {
                    started_tx.send("fast").expect("started");
                    2
                })
            })
        };
        let fast_started = started_rx.recv_timeout(Duration::from_secs(1));
        slow_gate.open();
        assert_eq!(fast_started, Ok("fast"));
        assert_eq!(fast.await.expect("fast task"), 2);
        assert_eq!(slow.await.expect("slow task"), 1);
        assert_eq!(cache.get(&"slow"), Some(1));
        assert!(cache.get(&"fast").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_leader_wakes_waiter_for_serial_retry() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();

        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_try_insert_with("key", || -> Result<i32, &'static str> {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    Err("first failed")
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");

        let waiter = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                cache.get_or_try_insert_with("key", || Ok::<i32, &'static str>(9))
            })
        };
        tokio::task::yield_now().await;
        gate.open();

        assert_eq!(leader.await.expect("leader task"), Err("first failed"));
        assert_eq!(waiter.await.expect("waiter task"), Ok(9));
        assert_eq!(cache.get(&"key"), Some(9));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panicking_leader_removes_flight_and_wakes_waiter() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_insert_with("key", || -> i32 {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    panic!("factory panic");
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");
        let waiter = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.get_or_insert_with("key", || 11) })
        };
        tokio::task::yield_now().await;
        gate.open();

        assert!(leader.await.expect_err("leader should panic").is_panic());
        assert_eq!(waiter.await.expect("waiter task"), 11);
        assert_eq!(cache.get(&"key"), Some(11));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn explicit_mutation_prevents_stale_flight_commit() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_insert_with("key", || {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    1
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");
        cache.insert("key", 99);
        gate.open();

        assert_eq!(leader.await.expect("leader task"), 1);
        assert_eq!(cache.get(&"key"), Some(99));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unrelated_insert_does_not_invalidate_an_active_flight() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(3).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_insert_with("key", || {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    1
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");
        cache.insert("other", 99);
        gate.open();

        assert_eq!(leader.await.expect("leader task"), 1);
        assert_eq!(cache.get(&"key"), Some(1));
        assert_eq!(cache.get(&"other"), Some(99));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn remove_prevents_a_stale_flight_commit() {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_insert_with("key", || {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    1
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");
        assert_eq!(cache.remove(&"key"), None);
        gate.open();

        assert_eq!(leader.await.expect("leader task"), 1);
        assert_eq!(cache.get(&"key"), None);
    }

    async fn assert_global_mutation_prevents_stale_flight_commit(
        mutate: impl FnOnce(&BlockingLruCache<&'static str, i32>),
    ) -> Arc<BlockingLruCache<&'static str, i32>> {
        let cache = Arc::new(BlockingLruCache::new(
            NonZeroUsize::new(2).expect("capacity"),
        ));
        let gate = Arc::new(Gate::default());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let leader = {
            let cache = Arc::clone(&cache);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                cache.get_or_insert_with("key", || {
                    started_tx.send(()).expect("started");
                    gate.wait();
                    1
                })
            })
        };
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("leader started");

        mutate(&cache);
        gate.open();

        assert_eq!(leader.await.expect("leader task"), 1);
        assert_eq!(cache.get(&"key"), None);
        cache
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clear_prevents_a_stale_flight_commit() {
        let cache =
            assert_global_mutation_prevents_stale_flight_commit(|cache| cache.clear()).await;
        assert_eq!(cache.get(&"other"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn with_mut_prevents_a_stale_flight_commit() {
        let cache = assert_global_mutation_prevents_stale_flight_commit(|cache| {
            cache.with_mut(|inner| {
                inner.put("other", 99);
            });
        })
        .await;
        assert_eq!(cache.get(&"other"), Some(99));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn direct_lock_prevents_a_stale_flight_commit() {
        let cache = assert_global_mutation_prevents_stale_flight_commit(|cache| {
            cache
                .blocking_lock()
                .expect("cache enabled inside runtime")
                .put("other", 99);
        })
        .await;
        assert_eq!(cache.get(&"other"), Some(99));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn accepts_non_clone_keys() {
        #[derive(Eq, Hash, PartialEq)]
        struct NonCloneKey(u8);

        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));
        assert_eq!(cache.get_or_insert_with(NonCloneKey(1), || 7), 7);
    }

    #[test]
    fn disabled_without_runtime() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));
        cache.insert("first", /*value*/ 1);
        assert!(cache.get(&"first").is_none());

        assert_eq!(cache.get_or_insert_with("first", || 2), 2);
        assert!(cache.get(&"first").is_none());

        assert!(cache.remove(&"first").is_none());
        cache.clear();

        let result = cache.with_mut(|inner| {
            inner.put("tmp", 3);
            inner.get(&"tmp").cloned()
        });
        assert_eq!(result, Some(3));
        assert!(cache.get(&"tmp").is_none());

        assert!(cache.blocking_lock().is_none());
    }
}
