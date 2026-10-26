use std::borrow::Borrow;
use std::hash::Hash;
use std::num::NonZeroUsize;

use lru::LruCache;
use sha1::Digest;
use sha1::Sha1;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;

/// A minimal LRU cache protected by a Tokio mutex.
/// Uncontended calls work with or without a Tokio runtime.
/// Unless a multi-thread runtime can support blocking, contended calls bypass
/// the cache. This cache is best-effort storage, not authoritative state.
pub struct BlockingLruCache<K, V> {
    inner: Mutex<LruCache<K, V>>,
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
        }
    }

    /// Returns a clone of the cached value for `key`, or computes and inserts it.
    /// Computation runs without the cache lock, so unrelated lookups and reentrant
    /// access can proceed. Concurrent misses may compute more than once; an entry
    /// inserted during computation wins over the computed value.
    pub fn get_or_insert_with(&self, key: K, value: impl FnOnce() -> V) -> V
    where
        V: Clone,
    {
        if let Some(v) = self.get(&key) {
            return v;
        }
        let v = value();
        if let Some(mut guard) = lock_if_runtime(&self.inner) {
            if let Some(v) = guard.get(&key) {
                return v.clone();
            }
            // Insert and return a clone to keep ownership in the cache.
            guard.put(key, v.clone());
        }
        v
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
        guard.put(key, value)
    }

    /// Removes the entry for `key` if it exists, returning it.
    pub fn remove<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut guard = lock_if_runtime(&self.inner)?;
        guard.pop(key)
    }

    /// Clears all entries from the cache.
    pub fn clear(&self) {
        if let Some(mut guard) = lock_if_runtime(&self.inner) {
            guard.clear();
        }
    }

    /// Executes `callback` on the stored cache, or returns `None` without calling
    /// it when contention cannot be resolved by blocking.
    pub fn with_mut<R>(&self, callback: impl FnOnce(&mut LruCache<K, V>) -> R) -> Option<R> {
        let mut guard = lock_if_runtime(&self.inner)?;
        Some(callback(&mut guard))
    }

    /// Provides direct access to the cache guard.
    /// Returns `None` on contention unless a multi-thread runtime supports blocking.
    pub fn blocking_lock(&self) -> Option<MutexGuard<'_, LruCache<K, V>>> {
        lock_if_runtime(&self.inner)
    }
}

fn lock_if_runtime<K, V>(m: &Mutex<LruCache<K, V>>) -> Option<MutexGuard<'_, LruCache<K, V>>>
where
    K: Eq + Hash,
{
    if let Ok(guard) = m.try_lock() {
        return Some(guard);
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
        Some(tokio::task::block_in_place(|| m.blocking_lock()))
    } else {
        // Blocking here can prevent the task holding the mutex from progressing,
        // and block_in_place panics on a current-thread runtime.
        None
    }
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

