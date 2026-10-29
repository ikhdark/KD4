//! Cooperative, process-local locks shared by one AgentControl tree.
//! These do not authorize or intercept filesystem/tool effects.

use crate::session::session::Session;
use codex_protocol::ThreadId;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct AgentResourceLocks {
    // Never held across an await, I/O, or another mutex acquisition.
    locks: Mutex<BTreeMap<String, HeldLock>>,
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
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, HeldLock>> {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn acquire(
        &self,
        session: &Arc<Session>,
        resource: String,
        cancellation: &CancellationToken,
    ) -> AgentLockResult {
        let mut locks = self.lock();
        // Check after entering the critical section: there is no queued async
        // acquisition that can later commit after its caller was cancelled.
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
            return AgentLockResult::new("acquired", Some(held.info.clone()), None);
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
                    "no acquisition was queued; do not busy-loop or hold resources needed by the owner. Use the agreed release/grant route and wait_agent with mailbox_only:true; finish blocked only if no viable release route exists",
                ),
            );
        }
        let info = AgentResourceLock {
            resource: resource.clone(),
            owner: session.thread_id,
            token: Uuid::now_v7(),
        };
        locks.insert(
            resource,
            HeldLock {
                info: info.clone(),
                runtime,
            },
        );
        AgentLockResult::new("acquired", Some(info), None)
    }

    pub(crate) fn release(
        &self,
        session: &Arc<Session>,
        resource: &str,
        token: Uuid,
    ) -> AgentLockResult {
        let mut locks = self.lock();
        let Some(held) = locks.get(resource) else {
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
        locks.remove(resource);
        AgentLockResult::new("released", Some(info), None)
    }

    pub(crate) fn list(&self) -> AgentLockResult {
        AgentLockResult {
            status: "listed",
            lock: None,
            locks: self.lock().values().map(|held| held.info.clone()).collect(),
            reason: None,
        }
    }

    pub(crate) fn recover(&self, resource: &str, token: Uuid) -> AgentLockResult {
        let held = {
            let locks = self.lock();
            let Some(held) = locks.get(resource) else {
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
        let mut locks = self.lock();
        match locks.get(resource) {
            None => AgentLockResult::new("not_found", None, None),
            Some(current)
                if current.info.token == token && current.runtime.ptr_eq(&held.runtime) =>
            {
                locks.remove(resource);
                AgentLockResult::new("recovered", Some(held.info), None)
            }
            Some(current) => AgentLockResult::new(
                "stale_token",
                Some(current.info.clone()),
                Some("ownership changed while checking drain proof; no lock was removed"),
            ),
        }
    }

    /// Linearizes the shutdown fence with acquisitions, without a new waiter.
    pub(crate) fn fence_session(&self, session: &Session) {
        let _locks = self.lock();
        session.shutting_down.store(true, Ordering::Release);
    }

    /// Called only by the existing owner after terminal/history/writer drain.
    /// Pointer identity prevents an old runtime's late cleanup deleting locks
    /// acquired by a resumed runtime with the same ThreadId.
    pub(crate) fn release_session(&self, session: &Session) {
        self.lock()
            .retain(|_, held| !std::ptr::eq(held.runtime.as_ptr(), session));
    }
}
