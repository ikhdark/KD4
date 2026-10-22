use super::*;
use crate::agent::AgentTreeClosingGuard;

/// Owned admission fence. Dropping before the durable commit restores admission;
/// after commit, UUIDs stay retired for this manager's lifetime.
pub struct ThreadRemovalGuard {
    state: Arc<ThreadManagerState>,
    ids: HashSet<ThreadId>,
    closing: HashMap<usize, AgentTreeClosingGuard>,
    committed: HashSet<ThreadId>,
}

impl ThreadRemovalGuard {
    pub fn commit(&mut self, deleted_ids: &[ThreadId]) {
        // A reparent may have removed an earlier discovered member before the
        // final plan. Retire only the exact committed deletion receipt.
        self.committed.extend(deleted_ids.iter().copied());
    }

    pub fn commit_thread(&mut self, id: ThreadId) {
        self.committed.insert(id);
    }

    async fn mark(&mut self, ids: &[ThreadId]) -> CodexResult<()> {
        // Publication uses the same lock order: thread map, then removal set.
        // Neither lock spans persistence or runtime shutdown.
        let threads = self.state.threads.write().await;
        let mut removing = self
            .state
            .removing_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ids
            .iter()
            .any(|id| removing.contains(id) && !self.ids.contains(id))
        {
            return Err(CodexErr::InvalidRequest(
                "overlapping subtree removal".into(),
            ));
        }
        removing.extend(ids.iter().copied());
        self.ids.extend(ids.iter().copied());
        drop(removing);
        for thread in threads.values() {
            let registry = thread.codex.session.services.agent_control.registry();
            self.closing
                .entry(Arc::as_ptr(registry) as usize)
                .or_insert_with(|| registry.begin_closing_agent_tree(ids[0]));
        }
        for closing in self.closing.values_mut() {
            closing.mark_threads(self.ids.iter().copied());
        }
        Ok(())
    }
}

impl Drop for ThreadRemovalGuard {
    fn drop(&mut self) {
        let mut removing = self
            .state
            .removing_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in self.ids.difference(&self.committed) {
            removing.remove(id);
        }
        // Registry closing guards retain pending failed admissions until cleanup drains.
    }
}

impl ThreadManager {
    /// Fence the actual session registries before selecting the final mixed subtree.
    /// A late publication is either discovered before its parent is fenced, or rejected.
    pub async fn begin_agent_subtree_removal(
        &self,
        root: ThreadId,
    ) -> CodexResult<(ThreadRemovalGuard, Vec<ThreadId>)> {
        let mut guard = ThreadRemovalGuard {
            state: Arc::clone(&self.state),
            ids: HashSet::new(),
            closing: HashMap::new(),
            committed: HashSet::new(),
        };
        guard.mark(&[root]).await?;
        loop {
            let ids = self.list_agent_subtree_thread_ids(root).await?;
            let new_ids = ids
                .iter()
                .filter(|id| !guard.ids.contains(id))
                .copied()
                .collect::<Vec<_>>();
            if new_ids.is_empty() {
                return Ok((guard, ids));
            }
            guard.mark(&new_ids).await?;
        }
    }
}
