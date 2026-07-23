use super::AgentControl;
use crate::agent::AgentStatus;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::watch;
use tracing::warn;

#[derive(Default)]
pub(super) struct V2Residency {
    state: Mutex<V2ResidencyState>,
}

#[derive(Default)]
struct V2ResidencyState {
    residents: VecDeque<ThreadId>,
    pending_slots: usize,
    evicting: HashMap<ThreadId, watch::Sender<bool>>,
}

pub(super) struct V2ResidencySlot {
    residency: Arc<V2Residency>,
    active: bool,
}

struct V2EvictionClaim {
    residency: Arc<V2Residency>,
    thread_id: ThreadId,
    active: bool,
}

enum UnloadOneResult {
    Reserved(V2ResidencySlot),
    Retry,
    Unavailable,
}

enum EvictionDisposition {
    ConvertToPending,
    RestoreResident,
    Discard,
}

impl V2ResidencySlot {
    pub(super) fn commit(mut self, thread_id: ThreadId) {
        self.residency.commit_slot(thread_id);
        self.active = false;
    }
}

impl Drop for V2ResidencySlot {
    fn drop(&mut self) {
        if self.active {
            self.residency.release_pending_slot();
        }
    }
}

impl AgentControl {
    pub(super) async fn reserve_v2_residency_slot(
        &self,
        state: &Arc<ThreadManagerState>,
        config: &Config,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        let capacity = config
            .effective_agent_max_threads(MultiAgentVersion::V2)
            .unwrap_or(usize::MAX);
        Arc::clone(&self.v2_residency)
            .reserve_slot(state, capacity, protected_thread_id)
            .await
    }

    pub(super) async fn touch_loaded_v2_residency(
        &self,
        state: &Arc<ThreadManagerState>,
        thread_id: ThreadId,
    ) -> bool {
        let Ok(thread) = state.get_thread(thread_id).await else {
            return false;
        };
        if !is_resident_candidate(thread.as_ref()) {
            return true;
        }
        self.v2_residency
            .wait_for_eviction_or_touch(thread_id)
            .await
    }

    pub(super) fn forget_v2_residency(&self, thread_id: ThreadId) {
        self.v2_residency.remove(thread_id);
    }
}

impl V2Residency {
    async fn reserve_slot(
        self: Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        capacity: usize,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        loop {
            if self.try_reserve_pending_slot(capacity) {
                return Ok(V2ResidencySlot {
                    residency: Arc::clone(&self),
                    active: true,
                });
            }
            match self
                .try_unload_one_resident(manager, protected_thread_id)
                .await
            {
                UnloadOneResult::Reserved(slot) => return Ok(slot),
                UnloadOneResult::Retry => continue,
                UnloadOneResult::Unavailable => {
                    return Err(CodexErr::AgentLimitReached {
                        max_threads: capacity,
                    });
                }
            }
        }
    }

    fn try_reserve_pending_slot(&self, capacity: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .residents
            .len()
            .saturating_add(state.pending_slots)
            .saturating_add(state.evicting.len())
            >= capacity
        {
            return false;
        }
        state.pending_slots += 1;
        true
    }

    async fn try_unload_one_resident(
        self: &Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        protected_thread_id: Option<ThreadId>,
    ) -> UnloadOneResult {
        let candidates_to_scan = self.resident_count();
        for _ in 0..candidates_to_scan {
            let Some(claim) = self.claim_lru_candidate(protected_thread_id) else {
                return UnloadOneResult::Unavailable;
            };
            let candidate_thread_id = claim.thread_id;
            let Some(candidate_thread) = manager
                .get_thread(candidate_thread_id)
                .await
                .ok()
                .filter(|thread| is_resident_candidate(thread))
            else {
                claim.discard();
                return UnloadOneResult::Retry;
            };
            if !is_unloadable(candidate_thread.as_ref()).await {
                claim.restore();
                continue;
            }
            candidate_thread.ensure_rollout_materialized().await;
            if let Err(err) = candidate_thread.shutdown_and_wait().await {
                warn!(
                    "failed to shut down v2 resident thread before unloading {candidate_thread_id}: {err}"
                );
                claim.restore();
                continue;
            }
            if manager
                .remove_thread_if_same(&candidate_thread_id, &candidate_thread)
                .await
            {
                return UnloadOneResult::Reserved(claim.into_pending_slot());
            }
            if manager
                .get_thread(candidate_thread_id)
                .await
                .is_ok_and(|thread| is_resident_candidate(thread.as_ref()))
            {
                claim.restore();
                continue;
            }
            claim.discard();
            return UnloadOneResult::Retry;
        }
        UnloadOneResult::Unavailable
    }

    fn resident_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .len()
    }

    fn claim_lru_candidate(
        self: &Arc<Self>,
        protected_thread_id: Option<ThreadId>,
    ) -> Option<V2EvictionClaim> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let candidates_to_scan = state.residents.len();
        for _ in 0..candidates_to_scan {
            let candidate_thread_id = state.residents.pop_front()?;
            if Some(candidate_thread_id) == protected_thread_id {
                state.residents.push_back(candidate_thread_id);
                continue;
            }
            let (completion, _) = watch::channel(false);
            state
                .evicting
                .insert(candidate_thread_id, completion.clone());
            return Some(V2EvictionClaim {
                residency: Arc::clone(self),
                thread_id: candidate_thread_id,
                active: true,
            });
        }
        None
    }

    async fn wait_for_eviction_or_touch(&self, thread_id: ThreadId) -> bool {
        let completion = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.evicting.get(&thread_id) {
                Some(completion) => Some(completion.subscribe()),
                None => {
                    touch_resident(&mut state.residents, thread_id);
                    None
                }
            }
        };
        let Some(mut completion) = completion else {
            return true;
        };
        let completed = *completion.borrow();
        if !completed {
            let _ = completion.changed().await;
        }
        false
    }

    fn remove(&self, thread_id: ThreadId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .retain(|resident_thread_id| *resident_thread_id != thread_id);
    }

    fn commit_slot(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        touch_resident(&mut state.residents, thread_id);
    }

    fn release_pending_slot(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
    }

    fn finish_eviction(&self, thread_id: ThreadId, disposition: EvictionDisposition) {
        let completion = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let completion = state.evicting.remove(&thread_id);
            if completion.is_some() {
                match disposition {
                    EvictionDisposition::ConvertToPending => {
                        state.pending_slots = state.pending_slots.saturating_add(1);
                    }
                    EvictionDisposition::RestoreResident => {
                        touch_resident(&mut state.residents, thread_id);
                    }
                    EvictionDisposition::Discard => {}
                }
            }
            completion
        };
        if let Some(completion) = completion {
            let _ = completion.send(true);
        }
    }
}

impl V2EvictionClaim {
    fn into_pending_slot(mut self) -> V2ResidencySlot {
        let residency = Arc::clone(&self.residency);
        self.finish(EvictionDisposition::ConvertToPending);
        V2ResidencySlot {
            residency,
            active: true,
        }
    }

    fn restore(mut self) {
        self.finish(EvictionDisposition::RestoreResident);
    }

    fn discard(mut self) {
        self.finish(EvictionDisposition::Discard);
    }

    fn finish(&mut self, disposition: EvictionDisposition) {
        if self.active {
            self.residency.finish_eviction(self.thread_id, disposition);
            self.active = false;
        }
    }
}

impl Drop for V2EvictionClaim {
    fn drop(&mut self) {
        self.finish(EvictionDisposition::RestoreResident);
    }
}

fn touch_resident(residents: &mut VecDeque<ThreadId>, thread_id: ThreadId) {
    residents.retain(|resident_thread_id| *resident_thread_id != thread_id);
    residents.push_back(thread_id);
}

fn is_resident_candidate(thread: &CodexThread) -> bool {
    thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        && is_v2_resident_session_source(&thread.session_source)
}

pub(super) fn is_v2_resident_session_source(session_source: &SessionSource) -> bool {
    matches!(session_source, SessionSource::SubAgent(_))
}

async fn is_unloadable(thread: &CodexThread) -> bool {
    matches!(
        thread.agent_status().await,
        AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Interrupted
    ) && thread.codex.session.active_turn.lock().await.is_none()
        && !thread
            .codex
            .session
            .input_queue
            .has_pending_mailbox_items()
            .await
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod tests;
