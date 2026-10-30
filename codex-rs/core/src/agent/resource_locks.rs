//! Cooperative, process-local locks shared by one AgentControl tree.
//! These do not authorize or intercept filesystem/tool effects.

use crate::session::session::Session;
use codex_protocol::ThreadId;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct AgentResourceLocks {
    // Never held across an await, I/O, or another mutex acquisition.
    state: Mutex<LockState>,
}

#[derive(Default)]
struct LockState {
    locks: BTreeMap<String, HeldLock>,
    waiters: BTreeMap<String, WaitQueue>,
    // Live wait:true calls and their resources, including notified retries. A
    // pending wait holds no tool-call gate, so this owner refuses its runtime's
    // acquisitions of other resources; with ordered acquisition, lock waits
    // stay acyclic. Same-resource calls join it and are idempotent.
    pending_waits: BTreeMap<Uuid, (Weak<Session>, String)>,
}

#[derive(Default)]
struct WaitQueue {
    queued: VecDeque<LockWaiter>,
    // One retry in flight, not an ownership reservation. A nonwaiting acquire
    // can still win; the notified caller must recheck under the owner mutex.
    notified: Option<Uuid>,
}

struct LockWaiter {
    id: Uuid,
    runtime: Weak<Session>,
    cancellation: CancellationToken,
    sender: oneshot::Sender<()>,
}

impl LockState {
    fn wake_runtime(&mut self, resource: &str, runtime: &Weak<Session>) {
        if let Some(queue) = self.waiters.get_mut(resource) {
            // Concurrent same-runtime acquires are idempotent too. Closing
            // these senders lets them observe the committed acquisition.
            queue
                .queued
                .retain(|waiter| !waiter.runtime.ptr_eq(runtime));
            if queue.queued.is_empty() && queue.notified.is_none() {
                self.waiters.remove(resource);
            }
        }
    }

    fn notify_next(&mut self, resource: &str) {
        if self.locks.contains_key(resource) {
            return;
        }
        let Some(queue) = self.waiters.get_mut(resource) else {
            return;
        };
        if queue.notified.is_some() {
            return;
        }
        while let Some(waiter) = queue.queued.pop_front() {
            // Never upgrade/drop a Session while holding this mutex.
            if !waiter.cancellation.is_cancelled()
                && waiter.runtime.strong_count() != 0
                && waiter.sender.send(()).is_ok()
            {
                queue.notified = Some(waiter.id);
                return;
            }
        }
        self.waiters.remove(resource);
    }
}

struct WaitRegistration<'a> {
    owner: &'a AgentResourceLocks,
    resource: String,
    id: Uuid,
}

impl Drop for WaitRegistration<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.lock();
        state.pending_waits.remove(&self.id);
        if let Some(queue) = state.waiters.get_mut(&self.resource) {
            queue.queued.retain(|waiter| waiter.id != self.id);
            if queue.notified == Some(self.id) {
                queue.notified = None;
            }
            if queue.queued.is_empty() && queue.notified.is_none() {
                state.waiters.remove(&self.resource);
            }
        }
        // Cancellation or a dropped future must not consume the only wakeup.
        state.notify_next(&self.resource);
    }
}

#[derive(Clone)]
struct HeldLock {
    info: AgentResourceLock,
    // A thread may be resumed into a new runtime. Thread identity alone must
    // not let that runtime release, inherit, or clean up the old one's locks.
    runtime: Weak<Session>,
}

#[derive(Clone, Serialize)]
pub(crate) struct AgentResourceLock {
    resource: String,
    owner: ThreadId,
    token: Uuid,
}

#[derive(Serialize)]
pub(crate) struct AgentLockResult {
    pub(crate) status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock: Option<AgentResourceLock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    locks: Vec<AgentResourceLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl AgentLockResult {
    fn new(
        status: &'static str,
        lock: Option<AgentResourceLock>,
        reason: Option<&'static str>,
    ) -> Self {
        Self {
            status,
            lock,
            locks: Vec::new(),
            reason,
        }
    }
}

impl AgentResourceLocks {
    fn lock(&self) -> MutexGuard<'_, LockState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn acquire(
        &self,
        session: &Arc<Session>,
        resource: String,
        cancellation: &CancellationToken,
    ) -> AgentLockResult {
        Self::acquire_locked(&mut self.lock(), session, resource, cancellation, None)
    }

    /// `deadline` bounds only the wait. Expiry removes the waiter and returns
    /// `timed_out` as data, never a thrown error or a later acquisition.
    pub(crate) async fn acquire_wait(
        &self,
        session: &Arc<Session>,
        resource: String,
        cancellation: &CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> AgentLockResult {
        let mut registration: Option<WaitRegistration<'_>> = None;
        loop {
            let (result, next) = {
                let mut state = self.lock();
                let result = Self::acquire_locked(
                    &mut state,
                    session,
                    resource.clone(),
                    cancellation,
                    registration.as_ref().map(|current| current.id),
                );
                let next = if result.status == "busy" {
                    let id = Uuid::now_v7();
                    let (sender, receiver) = oneshot::channel();
                    state
                        .waiters
                        .entry(resource.clone())
                        .or_default()
                        .queued
                        .push_back(LockWaiter {
                            id,
                            runtime: Arc::downgrade(session),
                            cancellation: cancellation.clone(),
                            sender,
                        });
                    state
                        .pending_waits
                        .insert(id, (Arc::downgrade(session), resource.clone()));
                    Some((
                        WaitRegistration {
                            owner: self,
                            resource: resource.clone(),
                            id,
                        },
                        receiver,
                    ))
                } else {
                    None
                };
                // Busy observation and subscription share release's mutex, so
                // release-before-subscription cannot strand this caller.
                (result, next)
            };
            // Keep the prior notification in flight through the atomic retry;
            // dropping it earlier could wake every waiter on the same release.
            drop(registration.take());
            let Some((guard, receiver)) = next else {
                return result;
            };
            registration = Some(guard);
            tracing::debug!(thread_id = %session.thread_id, resource = %resource, "agent_lock waiter registered");
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return AgentLockResult::new("cancelled", None, Some("acquisition was cancelled"));
                }
                signal = receiver => {
                    tracing::debug!(thread_id = %session.thread_id, resource = %resource, notified = signal.is_ok(), "agent_lock waiter retry");
                }
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    // Deregister first: its Drop retakes the owner mutex and
                    // passes on any in-flight wakeup.
                    drop(registration.take());
                    let holder = self.lock().locks.get(&resource).map(|held| held.info.clone());
                    return AgentLockResult::new(
                        "timed_out",
                        holder,
                        Some("the enclosing call deadline elapsed first; the waiter was removed and no ownership was granted. Retry in a later call; do not busy-loop"),
                    );
                }
            }
        }
    }

    fn acquire_locked(
        state: &mut LockState,
        session: &Arc<Session>,
        resource: String,
        cancellation: &CancellationToken,
        own_wait: Option<Uuid>,
    ) -> AgentLockResult {
        let locks = &mut state.locks;
        // Check inside the critical section, including every notified retry:
        // cancelled callers must never commit a queued acquisition later.
        if cancellation.is_cancelled() {
            return AgentLockResult::new("cancelled", None, Some("acquisition was cancelled"));
        }
        if session.shutting_down.load(Ordering::Acquire) {
            return AgentLockResult::new(
                "shutting_down",
                None,
                Some("draining agents cannot acquire locks"),
            );
        }
        let runtime = Arc::downgrade(session);
        if let Some(held) = locks.get(&resource)
            && held.runtime.ptr_eq(&runtime)
        {
            // Idempotent, not reference-counted: one matching release suffices.
            let info = held.info.clone();
            state.wake_runtime(&resource, &runtime);
            return AgentLockResult::new("acquired", Some(info), None);
        }
        // A pending wait is this runtime's next ordered acquisition. Until it
        // returns, only the same resource (its retry or an idempotent duplicate)
        // may proceed; holding another resource could close a wait cycle.
        if state.pending_waits.iter().any(|(id, (waiter, pending))| {
            Some(*id) != own_wait && waiter.ptr_eq(&runtime) && *pending != resource
        })
        {
            return AgentLockResult::new(
                "order_violation",
                None,
                Some(
                    "this runtime has a pending wait:true acquisition; no other acquisition can commit until it returns",
                ),
            );
        }
        if locks
            .values()
            .any(|held| held.runtime.ptr_eq(&runtime) && held.info.resource >= resource)
        {
            return AgentLockResult::new(
                "order_violation",
                None,
                Some(
                    "acquire new resources in strictly increasing case-sensitive UTF-8 order; release held locks before retrying a lower resource",
                ),
            );
        }
        if let Some(held) = locks.get(&resource) {
            return AgentLockResult::new(
                "busy",
                Some(held.info.clone()),
                Some(
                    "no acquisition was queued; retry acquire with wait:true for a targeted release wakeup. Do not busy-loop or hold resources needed by the owner; finish blocked only if no viable release route exists",
                ),
            );
        }
        let info = AgentResourceLock {
            resource: resource.clone(),
            owner: session.thread_id,
            token: Uuid::now_v7(),
        };
        locks.insert(
            resource.clone(),
            HeldLock {
                info: info.clone(),
                runtime: runtime.clone(),
            },
        );
        state.wake_runtime(&resource, &runtime);
        AgentLockResult::new("acquired", Some(info), None)
    }

    pub(crate) fn release(
        &self,
        session: &Arc<Session>,
        resource: &str,
        token: Uuid,
    ) -> AgentLockResult {
        let mut state = self.lock();
        let Some(held) = state.locks.get(resource) else {
            return AgentLockResult::new("not_found", None, None);
        };
        if held.info.owner != session.thread_id || !held.runtime.ptr_eq(&Arc::downgrade(session)) {
            return AgentLockResult::new(
                "not_owner",
                Some(held.info.clone()),
                Some("only the acquiring runtime may release this lock"),
            );
        }
        if held.info.token != token {
            return AgentLockResult::new(
                "stale_token",
                Some(held.info.clone()),
                Some("the acquisition token does not match the current lock"),
            );
        }
        let info = held.info.clone();
        // Owner, runtime, token and deletion share the same critical section.
        state.locks.remove(resource);
        state.notify_next(resource);
        AgentLockResult::new("released", Some(info), None)
    }

    pub(crate) fn list(&self) -> AgentLockResult {
        AgentLockResult {
            status: "listed",
            lock: None,
            locks: self
                .lock()
                .locks
                .values()
                .map(|held| held.info.clone())
                .collect(),
            reason: None,
        }
    }

    pub(crate) fn recover(&self, resource: &str, token: Uuid) -> AgentLockResult {
        let held = {
            let state = self.lock();
            let Some(held) = state.locks.get(resource) else {
                return AgentLockResult::new("not_found", None, None);
            };
            if held.info.token != token {
                return AgentLockResult::new(
                    "stale_token",
                    Some(held.info.clone()),
                    Some("ownership has changed; inspect the current lock"),
                );
            }
            held.clone()
        };
        // Never upgrade/drop a Session or inspect its lifecycle under the map
        // mutex. Missing runtimes and terminal status labels are not drain proof.
        let Some(session) = held.runtime.upgrade() else {
            return AgentLockResult::new(
                "recovery_blocked",
                Some(held.info),
                Some(
                    "holder runtime is unavailable without a confirmed drain receipt; retain the lock",
                ),
            );
        };
        if !session.shutting_down.load(Ordering::Acquire) {
            return AgentLockResult::new(
                "holder_active",
                Some(held.info),
                Some(
                    "holder has not shut down; an idle, interrupted, completed or errored turn is not agent termination",
                ),
            );
        }
        let retirement = session
            .persistence_retirement_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !matches!(retirement, Some(Ok(()))) {
            return AgentLockResult::new(
                "recovery_blocked",
                Some(held.info),
                Some(
                    "shutdown/drain is unconfirmed or failed; accepted producers or persistence may still be active; retain the lock until lifecycle cleanup completes",
                ),
            );
        }
        let mut state = self.lock();
        match state.locks.get(resource) {
            None => AgentLockResult::new("not_found", None, None),
            Some(current)
                if current.info.token == token && current.runtime.ptr_eq(&held.runtime) =>
            {
                state.locks.remove(resource);
                state.notify_next(resource);
                AgentLockResult::new("recovered", Some(held.info), None)
            }
            Some(current) => AgentLockResult::new(
                "stale_token",
                Some(current.info.clone()),
                Some("ownership changed while checking drain proof; no lock was removed"),
            ),
        }
    }

    /// Linearizes the shutdown fence with acquisitions and retires its waiters.
    pub(crate) fn fence_session(&self, session: &Session) {
        let mut state = self.lock();
        session.shutting_down.store(true, Ordering::Release);
        for queue in state.waiters.values_mut() {
            // Closing the sender wakes the pending acquisition to observe the
            // fence. An already-notified retry will observe the same fence.
            queue
                .queued
                .retain(|waiter| !std::ptr::eq(waiter.runtime.as_ptr(), session));
        }
        state
            .waiters
            .retain(|_, queue| !queue.queued.is_empty() || queue.notified.is_some());
    }

    /// Called only by the existing owner after terminal/history/writer drain.
    /// Pointer identity prevents an old runtime's late cleanup deleting locks
    /// acquired by a resumed runtime with the same ThreadId.
    pub(crate) fn release_session(&self, session: &Session) {
        let mut state = self.lock();
        let released: Vec<_> = state
            .locks
            .iter()
            .filter(|(_, held)| std::ptr::eq(held.runtime.as_ptr(), session))
            .map(|(resource, _)| resource.clone())
            .collect();
        for resource in released {
            state.locks.remove(&resource);
            state.notify_next(&resource);
        }
    }
}
