//! Bounded reuse of pure results after the owning path has revalidated access
//! and acquired its current dependencies. A read-only label, observation epoch,
//! similar query or unknown dependency is never an equivalence proof.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExactReuseKey<I, R, B, C> {
    pub(crate) operation: &'static str,
    pub(crate) inputs: I,
    pub(crate) source_revision: R,
    pub(crate) backend: B,
    pub(crate) configuration: C,
}

struct Entry<K, V> {
    key: K,
    value: Arc<V>,
    charge: usize,
}

pub(crate) struct ExactResultCache<K, V> {
    entries: Mutex<VecDeque<Entry<K, V>>>,
    max_entries: usize,
    max_bytes: usize,
}

impl<K: PartialEq, V> ExactResultCache<K, V> {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::default(),
            max_entries,
            max_bytes,
        }
    }

    /// `current` must come from the current authorized owning path, not a saved
    /// status receipt. Unknown revisions/backends/configuration use execution.
    pub(crate) fn get(&self, current: &K) -> Option<Arc<V>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = entries.iter().position(|entry| &entry.key == current)?;
        let entry = entries.remove(index)?;
        let value = Arc::clone(&entry.value);
        entries.push_back(entry);
        Some(value)
    }

    /// Owners admit only complete, successful pure results. No failures, live
    /// tasks, permissions, filesystem leases or producer effects are cached.
    pub(crate) fn insert(&self, key: K, value: Arc<V>, charge: usize) {
        if self.max_entries == 0 || charge > self.max_bytes {
            return;
        }
        let mut removed = Vec::new();
        {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(index) = entries.iter().position(|entry| entry.key == key) {
                removed.extend(entries.remove(index));
            }
            let mut bytes = entries.iter().map(|entry| entry.charge).sum::<usize>();
            while entries.len() >= self.max_entries || bytes.saturating_add(charge) > self.max_bytes
            {
                let Some(entry) = entries.pop_front() else {
                    break;
                };
                bytes = bytes.saturating_sub(entry.charge);
                removed.push(entry);
            }
            entries.push_back(Entry { key, value, charge });
        }
        // Never destroy an owner's values under the cache mutex. Computation,
        // admission and cancellation also stay outside it; misses do not wait
        // for another task or keep its lifetime alive.
        drop(removed);
    }
}
