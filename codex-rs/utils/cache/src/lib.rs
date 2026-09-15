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
    pub fn get_or_insert_with(&self, key: K, value: impl FnOnce() -> V) -> V
    where
        V: Clone,
    {
        if let Some(mut guard) = lock_if_runtime(&self.inner) {
            if let Some(v) = guard.get(&key) {
                return v.clone();
            }
            let v = value();
            // Insert and return a clone to keep ownership in the cache.
            guard.put(key, v.clone());
            return v;
        }
        value()
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

#[cfg(test)]
mod tests {
    use super::BlockingLruCache;
    use std::num::NonZeroUsize;

    #[tokio::test(flavor = "multi_thread")]
    async fn stores_and_retrieves_values() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));

        assert!(cache.get(&"first").is_none());
        cache.insert("first", /*value*/ 1);
        assert_eq!(cache.get(&"first"), Some(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_runtime_stores_and_retrieves_values() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));

        assert_eq!(cache.get_or_insert_with("first", || 1), 1);
        assert_eq!(cache.get_or_insert_with("first", || 2), 1);
        assert_eq!(cache.insert("first", 3), Some(1));
        assert_eq!(cache.get(&"first"), Some(3));
        assert_eq!(cache.remove(&"first"), Some(3));
        assert_eq!(cache.get(&"first"), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_runtime_skips_contended_cache_without_mutating_it() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));
        cache.insert("first", 1);
        let guard = cache.blocking_lock().expect("uncontended cache");

        assert_eq!(cache.get_or_insert_with("first", || 2), 2);
        assert_eq!(cache.insert("second", 3), None);
        assert_eq!(cache.remove(&"first"), None);
        cache.clear();
        assert_eq!(
            cache.with_mut(|_| panic!("contended callback must not run")),
            None::<()>
        );

        drop(guard);
        assert_eq!(cache.get(&"first"), Some(1));
        assert_eq!(cache.get(&"second"), None);
        assert_eq!(cache.get(&"third"), None);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn contended_multi_thread_callback_waits_and_mutates_the_cache() {
        // Run on the only worker so the guard-release task can progress only
        // after with_mut enters block_in_place.
        tokio::spawn(async {
            let cache = std::sync::Arc::new(BlockingLruCache::new(
                NonZeroUsize::new(2).expect("capacity"),
            ));
            cache.insert("first", 1);
            let holder_cache = std::sync::Arc::clone(&cache);
            let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let holder = tokio::spawn(async move {
                let guard = holder_cache.inner.lock().await;
                locked_tx.send(()).unwrap();
                release_rx.await.unwrap();
                drop(guard);
            });
            locked_rx.await.unwrap();
            assert!(cache.inner.try_lock().is_err());
            release_tx.send(()).unwrap();
            assert_eq!(cache.with_mut(|inner| inner.put("first", 2)), Some(Some(1)));
            holder.await.unwrap();
            assert_eq!(cache.get(&"first"), Some(2));
        })
        .await
        .unwrap();
    }

    #[test]
    fn stores_and_retrieves_values_without_runtime() {
        let cache = BlockingLruCache::new(NonZeroUsize::new(2).expect("capacity"));
        assert_eq!(cache.insert("first", 1), None);
        assert_eq!(cache.get(&"first"), Some(1));
        assert_eq!(cache.get_or_insert_with("first", || panic!("cache hit")), 1);
        assert_eq!(cache.get_or_insert_with("second", || 2), 2);
        assert_eq!(cache.get(&"second"), Some(2));
        assert_eq!(cache.remove(&"first"), Some(1));

        let result = cache.with_mut(|inner| {
            inner.put("tmp", 3);
            inner.get(&"tmp").cloned()
        });
        assert_eq!(result, Some(Some(3)));
        assert_eq!(cache.get(&"tmp"), Some(3));
        let guard = cache.blocking_lock().expect("uncontended cache");
        assert_eq!(guard.len(), 2);
        assert_eq!(
            cache.with_mut(|_| panic!("contended callback must not run")),
            None::<()>
        );
        drop(guard);
        cache.clear();
        assert_eq!(cache.get(&"second"), None);
        assert_eq!(cache.get(&"tmp"), None);
    }
}
