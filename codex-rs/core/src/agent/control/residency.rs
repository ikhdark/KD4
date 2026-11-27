use super::AgentControl;
use crate::agent::AgentStatus;
use crate::codex_thread::CodexThread;
use crate::codex_thread::StoppedThreadRetirement;
use crate::config::Config;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::ThreadRegistryLease;
use crate::thread_manager::V2ResidencyOwner;
use crate::thread_manager::V2ResidencyRegistration;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio::time::timeout;
use tokio::time::timeout_at;
use tracing::warn;

const RESIDENCY_MATERIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const RESIDENCY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const RESIDENCY_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct V2Residency {
    state: Mutex<V2ResidencyState>,
}

#[derive(Default)]
struct V2ResidencyState {
    residents: VecDeque<ResidentGeneration>,
    pending_slots: usize,
    evicting: HashMap<ThreadId, EvictingGeneration>,
}

#[derive(Clone)]
struct ResidentGeneration {
    thread_id: ThreadId,
    thread: Weak<CodexThread>,
    _registration: V2ResidencyRegistration,
}

impl ResidentGeneration {
    fn matches(&self, thread: &Arc<CodexThread>) -> bool {
        self.thread.ptr_eq(&Arc::downgrade(thread))
    }
}

struct EvictingGeneration {
    resident: ResidentGeneration,
    completion: Arc<watch::Sender<bool>>,
}

pub(super) struct V2ResidencySlot {
    residency: Arc<V2Residency>,
    active: bool,
}

struct V2EvictionClaim {
    residency: Arc<V2Residency>,
    resident: ResidentGeneration,
    completion: Arc<watch::Sender<bool>>,
    active: bool,
}

enum UnloadOneResult {
    Reserved(V2ResidencySlot),
    WaitingForLateShutdown,
    Retry,
    Unavailable,
}

enum CompletedUnloadResult {
    Reserved(V2ResidencySlot),
    Retry,
}

enum EvictionDisposition {
    ConvertToPending,
    RestoreResident,
    Discard,
}

impl V2ResidencySlot {
    pub(super) fn commit(
        mut self,
        lease: &ThreadRegistryLease,
        thread_id: ThreadId,
        thread: &Arc<CodexThread>,
    ) {
        let resident = self.residency.register_generation(lease, thread_id, thread);
        self.residency.commit_slot(resident);
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
        thread: &Arc<CodexThread>,
    ) -> bool {
        if !is_resident_candidate(thread) {
            return true;
        }
        let Some(completion) = state
            .with_current_thread_generation(thread_id, thread, |lease| {
                let resident = self.v2_residency.register_generation(lease, thread_id, thread);
                self.v2_residency.touch_or_watch_eviction(resident)
            })
            .await
        else {
            return false;
        };
        let Some(mut completion) = completion else {
            return true;
        };
        let completed = *completion.borrow_and_update();
        if !completed {
            let _ = completion.changed().await;
        }
        false
    }

    pub(crate) fn is_v2_resident_thread(thread: &CodexThread) -> bool {
        is_resident_candidate(thread)
    }

    // Call after observing the exact generation's stopped-thread retirement.
    // Nonresident callers keep their existing orderly-drain contract.
    pub(crate) fn confirm_stopped_v2_residency(thread: &CodexThread) -> CodexResult<()> {
        if !is_resident_candidate(thread) {
            return Ok(());
        }
        let session = &thread.codex.session;
        let retirement = session
            .persistence_retirement_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !session.terminal_tasks.is_closed()
            || !session.persistence_tasks.is_closed()
            || !matches!(retirement, Some(Ok(())))
        {
            return Err(CodexErr::Fatal(
                "stopped resident agent has no confirmed persistence retirement".into(),
            ));
        }
        let durability = session.rollout_durability_error.try_lock().map_err(|_| {
            CodexErr::Fatal("stopped resident agent's durable rollout barrier is pending".into())
        })?;
        if let Some(error) = durability.as_ref() {
            return Err(CodexErr::Fatal(format!(
                "stopped resident agent's durable rollout barrier failed: {error}"
            )));
        }
        Ok(())
    }
}

impl V2Residency {
    async fn reserve_slot(
        self: Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        capacity: usize,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        if capacity == 0 {
            return Err(CodexErr::AgentLimitReached {
                max_threads: capacity,
            });
        }
        let deadline = Instant::now() + RESIDENCY_ACQUISITION_TIMEOUT;
        let result = timeout_at(deadline, async {
        let mut attempted = HashSet::new();
        loop {
            if Instant::now() >= deadline {
                return Err(residency_acquisition_timeout());
            }
            if self.try_reserve_pending_slot(capacity, deadline) {
                return Ok(V2ResidencySlot {
                    residency: Arc::clone(&self),
                    active: true,
                });
            }
            match self
                .try_unload_one_resident(
                    manager,
                    protected_thread_id,
                    &mut attempted,
                    deadline,
                )
                .await
            {
                UnloadOneResult::Reserved(slot) => {
                    // An eviction transfers its charge atomically, but a role
                    // may now request less capacity than existing occupancy.
                    let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let fits = state.residents.len().saturating_add(state.pending_slots)
                        .saturating_add(state.evicting.len()) <= capacity;
                    drop(state);
                    if fits { return Ok(slot); }
                    drop(slot);
                    return Err(CodexErr::AgentLimitReached { max_threads: capacity });
                }
                UnloadOneResult::WaitingForLateShutdown => {
                    // Cleanup retains occupancy. An undelivered slot releases itself on drop.
                    return Err(CodexErr::AgentLimitReached {
                        max_threads: capacity,
                    });
                }
                UnloadOneResult::Retry => continue,
                UnloadOneResult::Unavailable => {
                    return Err(CodexErr::AgentLimitReached {
                        max_threads: capacity,
                    });
                }
            }
        }
        }).await;
        if Instant::now() >= deadline {
            return Err(residency_acquisition_timeout());
        }
        result.map_err(|_| residency_acquisition_timeout())?
    }

    fn try_reserve_pending_slot(&self, capacity: usize, deadline: Instant) -> bool {
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
        if Instant::now() >= deadline {
            return false;
        }
        state.pending_slots += 1;
        true
    }

    async fn try_unload_one_resident(
        self: &Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        protected_thread_id: Option<ThreadId>,
        attempted: &mut HashSet<ThreadId>,
        deadline: Instant,
    ) -> UnloadOneResult {
        let candidates_to_scan = self.resident_count();
        for _ in 0..candidates_to_scan {
            if Instant::now() >= deadline {
                return UnloadOneResult::Retry;
            }
            let Some(claim) = self.claim_lru_candidate(protected_thread_id, deadline) else {
                return UnloadOneResult::Unavailable;
            };
            let candidate_thread_id = claim.resident.thread_id;
            if !attempted.insert(candidate_thread_id) {
                claim.restore();
                continue;
            }
            let publication = manager
                .acquire_thread_registry_lease(candidate_thread_id)
                .await;
            if !self.is_current_claim(&claim) {
                return UnloadOneResult::Retry;
            }
            let Some(candidate_thread) = manager
                .get_thread(candidate_thread_id)
                .await
                .ok()
                .filter(|thread| claim.resident.matches(thread) && is_resident_candidate(thread))
            else {
                // A generic detach is not a drain receipt. Neither evict a
                // replacement nor release the old generation's charge here.
                claim.restore();
                continue;
            };
            if Instant::now() >= deadline {
                return UnloadOneResult::Retry;
            }
            // Idleness alone is not ownership: exclude newly accepted input
            // through materialization, irreversible shutdown and exact removal.
            let Some(residency_pin) = candidate_thread.try_pin_for_eviction() else {
                claim.restore();
                continue;
            };
            if !is_unloadable(candidate_thread.as_ref()).await {
                claim.restore();
                continue;
            }
            let now = Instant::now();
            if now >= deadline {
                return UnloadOneResult::Retry;
            }
            let materialize_deadline = deadline.min(now + RESIDENCY_MATERIALIZE_TIMEOUT);
            match timeout_at(
                materialize_deadline,
                candidate_thread.try_ensure_rollout_materialized(),
            )
            .await
            {
                Ok(Ok(())) if Instant::now() < materialize_deadline => {}
                Ok(Err(err)) => {
                    warn!("failed to materialize v2 resident thread {candidate_thread_id}: {err}");
                    claim.restore();
                    continue;
                }
                Ok(Ok(())) | Err(_) => {
                    warn!("timed out materializing v2 resident thread {candidate_thread_id}");
                    claim.restore();
                    continue;
                }
            }
            drop(publication);
            let (completion_tx, mut completion_rx) = oneshot::channel();
            let manager = Arc::clone(manager);
            if Instant::now() >= deadline {
                return UnloadOneResult::Retry;
            }
            // Transfer the claim before shutdown can be accepted. Cancellation of the
            // requester must never restore a runtime whose shutdown is underway.
            tokio::spawn(async move {
                let _residency_pin = residency_pin;
                let retirement = match candidate_thread.retire_stopped_generation().await {
                    Ok(retirement) => retirement,
                    Err(err) => {
                        warn!("v2 resident thread {candidate_thread_id} did not confirm retirement: {err}");
                        claim.restore();
                        let _ = completion_tx.send(CompletedUnloadResult::Retry);
                        return;
                    }
                };
                let result = finish_completed_unload(
                    &manager,
                    candidate_thread_id,
                    &candidate_thread,
                    &retirement,
                    claim,
                )
                .await;
                let _ = completion_tx.send(result);
            });
            return match timeout(RESIDENCY_SHUTDOWN_TIMEOUT, &mut completion_rx).await {
                Ok(Ok(CompletedUnloadResult::Reserved(slot))) => UnloadOneResult::Reserved(slot),
                Ok(Ok(CompletedUnloadResult::Retry)) | Ok(Err(_)) => UnloadOneResult::Retry,
                Err(_) => {
                    warn!(
                        "timed out shutting down v2 resident thread {candidate_thread_id}; cleanup retains occupancy"
                    );
                    UnloadOneResult::WaitingForLateShutdown
                }
            };
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
        deadline: Instant,
    ) -> Option<V2EvictionClaim> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let candidates_to_scan = state.residents.len();
        for _ in 0..candidates_to_scan {
            if Instant::now() >= deadline {
                return None;
            }
            let resident = state.residents.pop_front()?;
            if Some(resident.thread_id) == protected_thread_id
                || state.evicting.contains_key(&resident.thread_id)
            {
                state.residents.push_back(resident);
                continue;
            }
            let (completion, _) = watch::channel(false);
            let completion = Arc::new(completion);
            state.evicting.insert(
                resident.thread_id,
                EvictingGeneration {
                    resident: resident.clone(),
                    completion: Arc::clone(&completion),
                },
            );
            return Some(V2EvictionClaim {
                residency: Arc::clone(self),
                resident,
                completion,
                active: true,
            });
        }
        None
    }

    fn register_generation(
        self: &Arc<Self>,
        lease: &ThreadRegistryLease,
        thread_id: ThreadId,
        thread: &Arc<CodexThread>,
    ) -> ResidentGeneration {
        let owner: Arc<dyn V2ResidencyOwner> = self.clone();
        ResidentGeneration {
            thread_id,
            thread: Arc::downgrade(thread),
            _registration: lease.register_v2_residency_owner(Arc::downgrade(&owner)),
        }
    }

    fn touch_or_watch_eviction(&self, resident: ResidentGeneration) -> Option<watch::Receiver<bool>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(evicting) = state.evicting.get(&resident.thread_id)
            && evicting.resident.thread.ptr_eq(&resident.thread)
        {
            return Some(evicting.completion.subscribe());
        }
        touch_resident(&mut state.residents, resident);
        None
    }

    fn remove_matching(&self, thread_id: ThreadId, matches: impl Fn(&ResidentGeneration) -> bool) {
        let completion = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .residents
                .retain(|resident| resident.thread_id != thread_id || !matches(resident));
            if state
                .evicting
                .get(&thread_id)
                .is_some_and(|evicting| matches(&evicting.resident))
            {
                state
                    .evicting
                    .remove(&thread_id)
                    .map(|evicting| evicting.completion)
            } else {
                None
            }
        };
        if let Some(completion) = completion {
            let _ = completion.send(true);
        }
    }

    fn commit_slot(&self, resident: ResidentGeneration) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        if !state
            .evicting
            .get(&resident.thread_id)
            .is_some_and(|evicting| evicting.resident.thread.ptr_eq(&resident.thread))
        {
            touch_resident(&mut state.residents, resident);
        }
    }

    fn release_pending_slot(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
    }

    fn is_current_claim(&self, claim: &V2EvictionClaim) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .evicting
            .get(&claim.resident.thread_id)
            .is_some_and(|evicting| Arc::ptr_eq(&evicting.completion, &claim.completion))
    }

    fn finish_eviction(&self, claim: &V2EvictionClaim, disposition: EvictionDisposition) -> bool {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let thread_id = claim.resident.thread_id;
            if !state
                .evicting
                .get(&thread_id)
                .is_some_and(|evicting| Arc::ptr_eq(&evicting.completion, &claim.completion))
            {
                return false;
            }
            state.evicting.remove(&thread_id);
            match disposition {
                EvictionDisposition::ConvertToPending => {
                    state.pending_slots = state.pending_slots.saturating_add(1);
                }
                EvictionDisposition::RestoreResident => {
                    state.residents.push_back(claim.resident.clone());
                }
                EvictionDisposition::Discard => {}
            }
        }
        let _ = claim.completion.send(true);
        true
    }
}

impl V2ResidencyOwner for V2Residency {
    fn retire_resident_generation(&self, thread_id: ThreadId, expected: &Arc<CodexThread>) {
        self.remove_matching(thread_id, |resident| resident.matches(expected));
    }
}

async fn finish_completed_unload(
    manager: &Arc<ThreadManagerState>,
    candidate_thread_id: ThreadId,
    candidate_thread: &Arc<CodexThread>,
    retirement: &StoppedThreadRetirement,
    claim: V2EvictionClaim,
) -> CompletedUnloadResult {
    let mut claim = Some(claim);
    let mut slot = None;
    match manager
        .remove_stopped_thread_if_same(candidate_thread_id, candidate_thread, retirement, || {
            slot = claim.take().and_then(V2EvictionClaim::into_pending_slot);
        })
        .await
    {
        Ok(true) => {
            return slot.map_or(CompletedUnloadResult::Retry, CompletedUnloadResult::Reserved);
        }
        Ok(false) => {}
        Err(error) => {
            warn!(%candidate_thread_id, %error, "retaining unconfirmed v2 resident generation");
            return CompletedUnloadResult::Retry;
        }
    }
    // This claim's generation is confirmed drained even if another owner
    // detached it first. Never reattach it or change a replacement's charge.
    if let Some(claim) = claim {
        claim.discard();
    }
    CompletedUnloadResult::Retry
}

impl V2EvictionClaim {
    fn into_pending_slot(mut self) -> Option<V2ResidencySlot> {
        self.finish(EvictionDisposition::ConvertToPending)
            .then(|| V2ResidencySlot {
                residency: Arc::clone(&self.residency),
                active: true,
            })
    }

    fn restore(mut self) {
        self.finish(EvictionDisposition::RestoreResident);
    }

    fn discard(mut self) {
        self.finish(EvictionDisposition::Discard);
    }

    fn finish(&mut self, disposition: EvictionDisposition) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        self.residency.finish_eviction(self, disposition)
    }
}

impl Drop for V2EvictionClaim {
    fn drop(&mut self) {
        self.finish(EvictionDisposition::RestoreResident);
    }
}

fn residency_acquisition_timeout() -> CodexErr {
    CodexErr::UnsupportedOperation(
        "agent residency acquisition timed out; any shutdown in progress still owns its capacity".into(),
    )
}

fn touch_resident(residents: &mut VecDeque<ResidentGeneration>, resident: ResidentGeneration) {
    residents.retain(|current| {
        current.thread_id != resident.thread_id || !current.thread.ptr_eq(&resident.thread)
    });
    residents.push_back(resident);
}

fn is_resident_candidate(thread: &CodexThread) -> bool {
    thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        && is_v2_resident_session_source(&thread.session_source)
}

pub(super) fn is_v2_resident_session_source(session_source: &SessionSource) -> bool {
    matches!(session_source, SessionSource::SubAgent(_))
}

async fn is_unloadable(thread: &CodexThread) -> bool {
    let session = &thread.codex.session;
    if !matches!(
        thread.agent_status().await,
        AgentStatus::Completed(_)
            | AgentStatus::CompletedWithSurface { .. }
            | AgentStatus::Stopped { .. }
            | AgentStatus::Errored(_)
            | AgentStatus::Interrupted
    ) || session.active_turn.lock().await.is_some()
    {
        return false;
    }
    // Drop the active-turn observation before the queue's atomic recheck takes
    // that lock. Recovered user input is work too, not just mailbox traffic.
    // Non-trigger mail stays with the registry-retained InputQueue across eviction.
    !session
        .input_queue
        .has_pending_input(&session.active_turn)
        .await
}

