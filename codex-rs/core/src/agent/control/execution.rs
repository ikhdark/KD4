use super::AgentControl;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

type ExecutionPermitKey = (ThreadId, String);

#[derive(Default)]
pub(super) struct AgentExecutionLimiter {
    active: Arc<AtomicUsize>,
    max_threads: OnceLock<usize>,
    pending: Mutex<HashMap<ExecutionPermitKey, AgentExecutionGuard>>,
}

pub(crate) struct AgentExecutionGuard {
    active: Arc<AtomicUsize>,
}

pub(crate) struct AgentExecutionPermitRegistration {
    limiter: Arc<AgentExecutionLimiter>,
    key: Option<ExecutionPermitKey>,
}

pub(crate) struct AgentExecutionPermitCleanup {
    limiter: Arc<AgentExecutionLimiter>,
    key: Option<ExecutionPermitKey>,
}

pub(crate) struct AgentExecutionPermitThreadCleanup {
    limiter: Arc<AgentExecutionLimiter>,
    thread_id: ThreadId,
}

impl Drop for AgentExecutionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl AgentExecutionPermitRegistration {
    pub(crate) fn commit(mut self) {
        self.key = None;
    }
}

impl Drop for AgentExecutionPermitRegistration {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.limiter.remove_pending(&key);
        }
    }
}

impl Drop for AgentExecutionPermitCleanup {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.limiter.remove_pending(&key);
        }
    }
}

impl Drop for AgentExecutionPermitThreadCleanup {
    fn drop(&mut self) {
        self.limiter.remove_pending_for_thread(self.thread_id);
    }
}

impl AgentControl {
    pub(crate) async fn reserve_execution_capacity_for_op(
        &self,
        thread_id: ThreadId,
        op: &Op,
    ) -> CodexResult<Option<AgentExecutionGuard>> {
        self.reserve_execution_capacity_for_turn_start(thread_id, op_starts_turn(op))
            .await
    }

    pub(super) async fn reserve_execution_capacity_for_turn_start(
        &self,
        thread_id: ThreadId,
        starts_turn: bool,
    ) -> CodexResult<Option<AgentExecutionGuard>> {
        if !starts_turn {
            return Ok(None);
        }
        let state = self.upgrade()?;
        let thread = state.get_thread(thread_id).await?;
        if thread.codex.session.active_turn.lock().await.is_some() {
            return Ok(None);
        }
        let config = thread.codex.session.get_config().await;
        let multi_agent_version = thread
            .multi_agent_version()
            .unwrap_or_else(|| config.multi_agent_version_from_features());
        self.reserve_execution_capacity(multi_agent_version, &thread.session_source)
    }

    pub(crate) fn reserve_execution_capacity(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<Option<AgentExecutionGuard>> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(None);
        }
        Arc::clone(&self.agent_execution_limiter)
            .try_guard()
            .map(Some)
    }

    pub(crate) fn register_execution_permit(
        &self,
        thread_id: ThreadId,
        submission_id: &str,
        guard: Option<AgentExecutionGuard>,
    ) -> CodexResult<AgentExecutionPermitRegistration> {
        let limiter = Arc::clone(&self.agent_execution_limiter);
        let key = guard.map(|guard| {
            let key = (thread_id, submission_id.to_string());
            limiter.insert_pending(key.clone(), guard)?;
            Ok::<_, CodexErr>(key)
        });
        let key = match key {
            Some(key) => Some(key?),
            None => None,
        };
        Ok(AgentExecutionPermitRegistration { limiter, key })
    }

    pub(crate) fn execution_permit_cleanup(
        &self,
        thread_id: ThreadId,
        submission_id: &str,
    ) -> AgentExecutionPermitCleanup {
        AgentExecutionPermitCleanup {
            limiter: Arc::clone(&self.agent_execution_limiter),
            key: Some((thread_id, submission_id.to_string())),
        }
    }

    pub(crate) fn execution_permit_thread_cleanup(
        &self,
        thread_id: ThreadId,
    ) -> AgentExecutionPermitThreadCleanup {
        AgentExecutionPermitThreadCleanup {
            limiter: Arc::clone(&self.agent_execution_limiter),
            thread_id,
        }
    }

    pub(crate) fn execution_guard_for_task(
        &self,
        thread_id: ThreadId,
        submission_id: &str,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<Option<AgentExecutionGuard>> {
        if let Some(guard) = self
            .agent_execution_limiter
            .take_pending(thread_id, submission_id)
        {
            return Ok(Some(guard));
        }
        self.reserve_execution_capacity(multi_agent_version, session_source)
    }
}

impl AgentExecutionLimiter {
    pub(super) fn initialize(&self, max_threads: usize) {
        self.max_threads.get_or_init(|| max_threads);
    }

    fn max_threads(&self) -> usize {
        self.max_threads.get().copied().unwrap_or(usize::MAX)
    }

    fn try_guard(self: Arc<Self>) -> CodexResult<AgentExecutionGuard> {
        let max_threads = self.max_threads();
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                if active < max_threads {
                    Some(active + 1)
                } else {
                    None
                }
            })
            .map_err(|_| CodexErr::AgentLimitReached { max_threads })?;
        Ok(AgentExecutionGuard {
            active: Arc::clone(&self.active),
        })
    }

    fn insert_pending(
        &self,
        key: ExecutionPermitKey,
        guard: AgentExecutionGuard,
    ) -> CodexResult<()> {
        match self.pending().entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(guard);
                Ok(())
            }
            Entry::Occupied(entry) => Err(CodexErr::Fatal(format!(
                "execution permit already registered for submission {} on thread {}",
                entry.key().1,
                entry.key().0
            ))),
        }
    }

    fn take_pending(
        &self,
        thread_id: ThreadId,
        submission_id: &str,
    ) -> Option<AgentExecutionGuard> {
        self.pending()
            .remove(&(thread_id, submission_id.to_string()))
    }

    fn remove_pending(&self, key: &ExecutionPermitKey) {
        self.pending().remove(key);
    }

    fn remove_pending_for_thread(&self, thread_id: ThreadId) {
        self.pending()
            .retain(|(pending_thread_id, _), _| *pending_thread_id != thread_id);
    }

    fn pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<ExecutionPermitKey, AgentExecutionGuard>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn op_starts_turn(op: &Op) -> bool {
    matches!(op, Op::UserInput { .. })
        || matches!(op, Op::InterAgentCommunication { communication } if communication.trigger_turn)
}

fn is_execution_limited(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(session_source, SessionSource::SubAgent(_))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;
