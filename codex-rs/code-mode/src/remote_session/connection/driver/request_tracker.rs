use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use tokio::sync::mpsc;

use super::ExecuteClaims;
use super::types::DeferredWait;
use super::types::DriverEvent;
use super::types::InitialResponse;
use super::types::PendingRequest;
use super::types::RemoteSession;
use super::types::UnclaimedExecute;

pub(super) enum CancellationAction {
    Send(RequestId),
    Terminate {
        request_id: RequestId,
        execute: UnclaimedExecute,
    },
}

struct ControlDeadline {
    at: tokio::time::Instant,
    budget: std::time::Duration,
    extensions: u32,
    // Backend stall observed since this window was armed at `armed_at`; a
    // stall before it existed is not evidence for this window.
    armed_at: tokio::time::Instant,
    stalled: std::time::Duration,
}

pub(super) struct RequestTracker {
    pending: HashMap<RequestId, PendingRequest>,
    control_deadlines: HashMap<RequestId, ControlDeadline>,
    unclaimed_executes: HashMap<RequestId, UnclaimedExecute>,
    execute_claims: Arc<ExecuteClaims>,
    initial_responses: HashMap<RequestId, InitialResponse>,
    deferred_waits: VecDeque<DeferredWait>,
    next_request_id: i64,
}

impl RequestTracker {
    pub(super) fn new(execute_claims: Arc<ExecuteClaims>) -> Self {
        Self {
            pending: HashMap::new(),
            control_deadlines: HashMap::new(),
            unclaimed_executes: HashMap::new(),
            execute_claims,
            initial_responses: HashMap::new(),
            deferred_waits: VecDeque::new(),
            next_request_id: 1,
        }
    }

    pub(super) fn contains_pending_open(&self, session: &RemoteSession) -> bool {
        self.pending.values().any(|pending| {
            matches!(
                pending,
                PendingRequest::OpenSession {
                    session: pending_session,
                    ..
                } if pending_session.id == session.id
            )
        })
    }

    pub(super) fn allocate_id(&mut self) -> Result<RequestId, String> {
        let id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| "code-mode host request ID space exhausted".to_string())?;
        Ok(RequestId::new(id))
    }

    pub(super) fn insert_pending(
        &mut self,
        id: RequestId,
        pending: PendingRequest,
        event_tx: &mpsc::Sender<DriverEvent>,
    ) {
        assert!(
            !self.pending.contains_key(&id)
                && !self.initial_responses.contains_key(&id)
                && !self.unclaimed_executes.contains_key(&id),
            "cannot reuse a live code-mode request ID: {id:?}"
        );
        let timeout = match &pending {
            PendingRequest::Terminate { .. } => Some(super::super::TERMINATE_TIMEOUT),
            PendingRequest::ShutdownSession { .. } => Some(super::super::SHUTDOWN_SESSION_TIMEOUT),
            _ => None,
        };
        if let Some(timeout) = timeout {
            let armed_at = tokio::time::Instant::now();
            self.control_deadlines.insert(
                id,
                ControlDeadline {
                    at: armed_at + timeout,
                    budget: timeout,
                    extensions: 0,
                    armed_at,
                    stalled: std::time::Duration::ZERO,
                },
            );
        }
        let replaced = self.pending.insert(id, pending);
        debug_assert!(replaced.is_none());
        if let Some(cancellation) = self
            .pending
            .get_mut(&id)
            .and_then(PendingRequest::cancellation_mut)
        {
            cancellation.spawn_watcher(id, event_tx.clone());
        }
    }

    pub(super) fn remove_pending(&mut self, id: RequestId) -> Option<PendingRequest> {
        self.control_deadlines.remove(&id);
        self.pending.remove(&id)
    }

    pub(super) fn next_control_deadline(&self) -> Option<tokio::time::Instant> {
        self.control_deadlines.values().map(|deadline| deadline.at).min()
    }

    /// Charges a backend stall measured up to `now` to every armed control
    /// window, clipped to the part of it that window existed for.
    pub(super) fn note_control_stall(
        &mut self,
        stalled: std::time::Duration,
        now: tokio::time::Instant,
    ) {
        if stalled.is_zero() {
            return;
        }
        for deadline in self.control_deadlines.values_mut() {
            let charged = stalled.min(now.saturating_duration_since(deadline.armed_at));
            deadline.stalled = deadline.stalled.saturating_add(charged);
        }
    }

    /// Re-arms every control deadline expired at `now` for a full budget when
    /// its own window saw at least `BACKEND_STALL_EVIDENCE` of backend stall.
    /// Returns the largest re-armed stall (zero when none had expired, e.g.
    /// it was acknowledged meanwhile), or `None` when an expired window has no
    /// stall evidence or no re-arm left.
    pub(super) fn rearm_expired_control_deadlines(
        &mut self,
        now: tokio::time::Instant,
    ) -> Option<std::time::Duration> {
        let expired: Vec<&mut ControlDeadline> = self
            .control_deadlines
            .values_mut()
            .filter(|deadline| deadline.at <= now)
            .collect();
        if expired.iter().any(|deadline| {
            deadline.stalled < super::super::BACKEND_STALL_EVIDENCE
                || deadline.extensions >= super::super::MAX_BACKEND_STALL_EXTENSIONS
        }) {
            return None;
        }
        let mut largest = std::time::Duration::ZERO;
        for deadline in expired {
            largest = largest.max(deadline.stalled);
            deadline.extensions += 1;
            deadline.stalled = std::time::Duration::ZERO;
            deadline.armed_at = now;
            deadline.at = now + deadline.budget;
        }
        Some(largest)
    }

    pub(super) fn insert_initial_response(&mut self, id: RequestId, response: InitialResponse) {
        assert!(
            !self.pending.contains_key(&id)
                && !self.initial_responses.contains_key(&id)
                && !self.unclaimed_executes.contains_key(&id),
            "cannot install a duplicate or premature initial response: {id:?}"
        );
        let replaced = self.initial_responses.insert(id, response);
        debug_assert!(replaced.is_none());
        // ExecutionStarted reserves this ID before sending the execute result
        // to the caller, which can claim it before the unclaimed row is inserted.
        self.execute_claims.register(id);
    }

    pub(super) fn remove_initial_response(&mut self, id: RequestId) -> Option<InitialResponse> {
        let response = self.initial_responses.remove(&id);
        if !self.unclaimed_executes.contains_key(&id) {
            self.execute_claims.retire(id);
        }
        response
    }

    pub(super) fn insert_unclaimed_execute(&mut self, id: RequestId, execute: UnclaimedExecute) {
        assert!(
            !self.pending.contains_key(&id)
                && self.initial_responses.contains_key(&id)
                && !self.unclaimed_executes.contains_key(&id),
            "cannot install a duplicate or premature execute result: {id:?}"
        );
        let replaced = self.unclaimed_executes.insert(id, execute);
        debug_assert!(replaced.is_none());
    }

    pub(super) fn claim_execute(&mut self, id: RequestId) {
        self.unclaimed_executes.remove(&id);
    }

    pub(super) fn collect_cancellations(&mut self) -> Vec<CancellationAction> {
        let mut actions = self
            .pending
            .iter_mut()
            .filter_map(|(id, pending)| {
                let cancellation = pending.cancellation_mut()?;
                (cancellation.is_cancelled() && cancellation.mark_reported())
                    .then_some(CancellationAction::Send(*id))
            })
            .collect::<Vec<_>>();
        let claims = &self.execute_claims;
        actions.extend(
            self.unclaimed_executes
                .extract_if(|_, execute| {
                    execute.cancellation.is_cancelled() && execute.cancellation.mark_reported()
                })
                .map(|(request_id, execute)| {
                    claims.retire(request_id);
                    CancellationAction::Terminate {
                        request_id,
                        execute,
                    }
                }),
        );
        actions
    }

    pub(super) fn mark_cancelled(&mut self, id: RequestId) -> Option<CancellationAction> {
        if let Some(cancellation) = self
            .pending
            .get_mut(&id)
            .and_then(PendingRequest::cancellation_mut)
        {
            return cancellation
                .mark_reported()
                .then_some(CancellationAction::Send(id));
        }
        let execute = self.unclaimed_executes.get_mut(&id)?;
        if !execute.cancellation.mark_reported() {
            return None;
        }
        self.execute_claims.retire(id);
        self.unclaimed_executes
            .remove(&id)
            .map(|execute| CancellationAction::Terminate {
                request_id: id,
                execute,
            })
    }

    pub(super) fn has_cancelled_wait(&self, session: &RemoteSession, cell_id: &WireCellId) -> bool {
        self.pending.values().any(|pending| {
            matches!(
                pending,
                PendingRequest::Wait {
                    session: pending_session,
                    cell_id: pending_cell_id,
                    cancellation,
                    ..
                } if pending_session == session
                    && pending_cell_id == cell_id
                    && cancellation.is_cancelled()
            )
        })
    }

    pub(super) fn push_deferred_wait(&mut self, wait: DeferredWait) {
        self.deferred_waits.push_back(wait);
    }

    pub(super) fn take_deferred_waits(&mut self) -> VecDeque<DeferredWait> {
        std::mem::take(&mut self.deferred_waits)
    }

    pub(super) fn remove_unclaimed_for_session(&mut self, session_id: &SessionId) {
        let claims = &self.execute_claims;
        self.unclaimed_executes.retain(|id, execute| {
            if &execute.session.id != session_id {
                return true;
            }
            claims.retire(*id);
            false
        });
    }

    pub(super) fn operation_count(&self) -> usize {
        // A started execute can have both an initial response and a claim
        // outstanding. Count it once, until the last obligation is settled.
        let claim_only = self
            .unclaimed_executes
            .keys()
            .filter(|id| !self.initial_responses.contains_key(*id))
            .count();
        self.pending.len() + self.initial_responses.len() + self.deferred_waits.len() + claim_only
    }

    pub(super) fn fail_all(&mut self, reason: &str) {
        self.execute_claims.close();
        self.control_deadlines.clear();
        for (_, pending) in self.pending.drain() {
            pending.fail(reason.to_string());
        }
        self.unclaimed_executes.clear();
        for (_, initial) in self.initial_responses.drain() {
            let _ = initial.response_tx.send(Err(reason.to_string()));
        }
        for wait in self.deferred_waits.drain(..) {
            let _ = wait.response_tx.send(Err(reason.to_string()));
        }
    }
}
