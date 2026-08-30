//! Turn-scoped state and active turn metadata scaffolding.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Span;

use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::protocol::SamplingGenerationId;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_sandboxing::policy_transforms::merge_uri_permission_profiles;
use rmcp::model::RequestId;
use tokio::sync::oneshot;

use crate::agent::control::AgentExecutionGuard;
use crate::session::TurnInputQueue;
use crate::session::reasoning_governor::ReasoningPolicyRecorder;
use crate::session::turn_context::TurnContext;
use crate::tasks::SessionTask;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::TokenUsage;

/// Metadata about the currently running turn.
pub(crate) struct ActiveTurn {
    pub(crate) task: Option<RunningTask>,
    pub(crate) turn_state: Arc<Mutex<TurnState>>,
    pub(crate) terminal: Option<Arc<TurnTerminalCoordinator>>,
    pub(crate) reasoning_policy_recorder: Arc<ReasoningPolicyRecorder>,
}

/// Whether mailbox deliveries should still be folded into the current turn.
///
/// State machine:
/// - A turn starts in `CurrentTurn`, so queued child mail can join the next
///   model request for that turn.
/// - After user-visible terminal output is recorded, we switch to `NextTurn`
///   to leave late child mail queued instead of extending an already shown
///   answer.
/// - If the same task later gets explicit same-turn work again (a steered user
///   prompt or a tool call after an untagged preamble), we reopen `CurrentTurn`
///   so that pending child mail is drained into that follow-up request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MailboxDeliveryPhase {
    /// Incoming mailbox messages can still be consumed by the current turn.
    #[default]
    CurrentTurn,
    /// The current turn already emitted visible final answer text; mailbox
    /// messages should remain queued for a later turn.
    NextTurn,
}

impl Default for ActiveTurn {
    fn default() -> Self {
        Self {
            task: None,
            turn_state: Arc::new(Mutex::new(TurnState::default())),
            terminal: None,
            reasoning_policy_recorder: Arc::new(ReasoningPolicyRecorder::new(false)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskKind {
    Regular,
    Review,
    Compact,
}

pub(crate) struct RunningTask {
    pub(crate) done: Arc<Notify>,
    /// Level-triggered completion state paired with `done`. `Notify` alone is
    /// edge-triggered and its retained permit can be consumed by an earlier
    /// finalizer before a fail-safe verifies that the worker dropped.
    pub(crate) worker_done: Arc<AtomicBool>,
    pub(crate) kind: TaskKind,
    pub(crate) task: Arc<dyn SessionTask>,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) auxiliary_cancellation_token: CancellationToken,
    pub(crate) auxiliary_tasks: JoinSet<()>,
    pub(crate) worker_abort_handle: AbortHandle,
    pub(crate) _supervisor_handle: JoinHandle<()>,
    pub(crate) task_span: Span,
    pub(crate) turn_context: Arc<TurnContext>,
    pub(crate) _agent_execution_guard: Option<AgentExecutionGuard>,
}

#[derive(Debug)]
pub(crate) struct TurnTerminalCoordinator {
    turn_id: String,
    terminal_decision_gate: std::sync::Mutex<()>,
    claimed: AtomicBool,
    analytics_emission_claimed: AtomicBool,
    interaction_released: AtomicBool,
    cleanup_completed: AtomicBool,
    completion_notify: Notify,
    cleanup_completion_notify: Notify,
    interrupt_fence_state: AtomicU8,
    interrupt_resolution_notify: Notify,
    sampling_admission_gate: Arc<Mutex<()>>,
    tool_call_acceptance: Arc<ToolCallAcceptanceGate>,
    wake_generation: AtomicU64,
    completion_waiters: AtomicU32,
    #[cfg(test)]
    cleanup_waiters: AtomicU32,
    interrupt_resolution_waiters: AtomicU32,
    sampling_admission_waiters: AtomicU32,
    #[cfg(test)]
    panic_before_worker_cancellation: AtomicBool,
}

/// The interrupt durability fence is one state machine, not three independent
/// facts. In particular, terminal admission is legal only after the interrupted
/// output is durable (or its persistence attempt has definitively failed), while
/// provider sampling stays fenced until terminal cleanup releases the state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum InterruptFenceState {
    Open = 0,
    PendingPersistence = 1,
    OutputDurable = 2,
    PersistenceFailed = 3,
}

/// Serializes tool-call admission against terminal sealing. Timing observes
/// accepted calls, but it does not own this lifecycle decision.
#[derive(Debug, Default)]
pub(crate) struct ToolCallAcceptanceGate {
    sealed: std::sync::Mutex<bool>,
}

impl ToolCallAcceptanceGate {
    pub(crate) fn try_accept(&self, accept: impl FnOnce() -> bool) -> bool {
        let sealed = self
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !*sealed && accept()
    }

    fn seal(&self, record_seal: impl FnOnce()) {
        let mut sealed = self
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*sealed {
            *sealed = true;
            record_seal();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SamplingAdmission {
    Allowed,
    Fenced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalWakeResult {
    Applied,
    Stale,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalWaiterSnapshot {
    pub(crate) completion: u32,
    pub(crate) cleanup: u32,
    pub(crate) interrupt_resolution: u32,
    pub(crate) sampling_admission: u32,
}

struct TerminalWaiterGuard<'a>(&'a AtomicU32);

impl<'a> TerminalWaiterGuard<'a> {
    fn new(counter: &'a AtomicU32) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for TerminalWaiterGuard<'_> {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "terminal waiter count underflow");
    }
}

impl InterruptFenceState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::PendingPersistence,
            2 => Self::OutputDurable,
            3 => Self::PersistenceFailed,
            _ => Self::Open,
        }
    }

    fn is_fenced(self) -> bool {
        self != Self::Open
    }

    fn terminal_ready(self) -> bool {
        matches!(self, Self::OutputDurable | Self::PersistenceFailed)
    }
}

impl TurnTerminalCoordinator {
    #[cfg(test)]
    pub(crate) fn new(turn_id: String) -> Arc<Self> {
        Self::new_with_tool_call_acceptance(turn_id, Arc::new(ToolCallAcceptanceGate::default()))
    }

    pub(crate) fn new_with_tool_call_acceptance(
        turn_id: String,
        tool_call_acceptance: Arc<ToolCallAcceptanceGate>,
    ) -> Arc<Self> {
        Arc::new(Self {
            turn_id,
            terminal_decision_gate: std::sync::Mutex::new(()),
            claimed: AtomicBool::new(false),
            analytics_emission_claimed: AtomicBool::new(false),
            interaction_released: AtomicBool::new(false),
            cleanup_completed: AtomicBool::new(false),
            completion_notify: Notify::new(),
            cleanup_completion_notify: Notify::new(),
            interrupt_fence_state: AtomicU8::new(InterruptFenceState::Open as u8),
            interrupt_resolution_notify: Notify::new(),
            sampling_admission_gate: Arc::new(Mutex::new(())),
            tool_call_acceptance,
            wake_generation: AtomicU64::new(1),
            completion_waiters: AtomicU32::new(0),
            #[cfg(test)]
            cleanup_waiters: AtomicU32::new(0),
            interrupt_resolution_waiters: AtomicU32::new(0),
            sampling_admission_waiters: AtomicU32::new(0),
            #[cfg(test)]
            panic_before_worker_cancellation: AtomicBool::new(false),
        })
    }

    pub(crate) fn seal_tool_call_acceptance(&self, timing: &crate::turn_timing::TurnTimingState) {
        self.tool_call_acceptance
            .seal(|| timing.record_tool_call_acceptance_closed());
    }

    pub(crate) fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub(crate) fn wake_generation_id(&self) -> SamplingGenerationId {
        SamplingGenerationId(format!(
            "terminal-{}-{}",
            self.turn_id,
            self.wake_generation.load(Ordering::Acquire)
        ))
    }

    #[cfg(test)]
    pub(crate) fn waiter_snapshot(&self) -> TerminalWaiterSnapshot {
        TerminalWaiterSnapshot {
            completion: self.completion_waiters.load(Ordering::Acquire),
            cleanup: self.cleanup_waiters.load(Ordering::Acquire),
            interrupt_resolution: self.interrupt_resolution_waiters.load(Ordering::Acquire),
            sampling_admission: self.sampling_admission_waiters.load(Ordering::Acquire),
        }
    }

    fn wake_generation_matches(&self, expected: &SamplingGenerationId) -> bool {
        &self.wake_generation_id() == expected
    }

    pub(crate) fn try_claim(self: &Arc<Self>) -> Option<TurnTerminalPermit> {
        let _decision = self
            .terminal_decision_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interrupt_fence = self.interrupt_fence_state();
        if interrupt_fence.is_fenced() && !interrupt_fence.terminal_ready() {
            return None;
        }
        self.claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(TurnTerminalPermit {
            coordinator: Arc::clone(self),
            cleanup_completed: false,
        })
    }

    pub(crate) fn interaction_released(&self) -> bool {
        self.interaction_released.load(Ordering::Acquire)
    }

    pub(crate) fn mark_interaction_released(&self) {
        self.interaction_released.store(true, Ordering::Release);
        self.completion_notify.notify_waiters();
    }

    /// Establish a pre-terminal fence. This deliberately does not claim or
    /// terminalize the turn; it only closes provider sampling admission.
    pub(crate) async fn mark_interrupt_pending(&self) -> bool {
        let _waiter = TerminalWaiterGuard::new(&self.sampling_admission_waiters);
        let _admission_gate = self.sampling_admission_gate.lock().await;
        let _decision = self
            .terminal_decision_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.claimed.load(Ordering::Acquire) {
            return false;
        }
        self.wake_generation.fetch_add(1, Ordering::AcqRel);
        self.interrupt_fence_state.store(
            InterruptFenceState::PendingPersistence as u8,
            Ordering::Release,
        );
        true
    }

    pub(crate) async fn acquire_sampling_admission(&self) -> Option<OwnedMutexGuard<()>> {
        let _waiter = TerminalWaiterGuard::new(&self.sampling_admission_waiters);
        let guard = Arc::clone(&self.sampling_admission_gate).lock_owned().await;
        if self.interrupt_pending() {
            None
        } else {
            Some(guard)
        }
    }

    pub(crate) fn sampling_admission(&self) -> SamplingAdmission {
        if self.interrupt_fence_state().is_fenced() {
            SamplingAdmission::Fenced
        } else {
            SamplingAdmission::Allowed
        }
    }

    pub(crate) fn interrupt_pending(&self) -> bool {
        self.interrupt_fence_state().is_fenced()
    }

    /// Open terminal admission after the interrupted tool output has crossed
    /// its durability barrier. Sampling remains fenced until cleanup.
    pub(crate) fn mark_interrupt_output_durable(
        &self,
        expected_generation: &SamplingGenerationId,
    ) -> TerminalWakeResult {
        let _decision = self
            .terminal_decision_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.wake_generation_matches(expected_generation) {
            return TerminalWakeResult::Stale;
        }
        if self.interrupt_fence_state() == InterruptFenceState::PendingPersistence {
            self.interrupt_fence_state
                .store(InterruptFenceState::OutputDurable as u8, Ordering::Release);
        }
        self.interrupt_resolution_notify.notify_waiters();
        TerminalWakeResult::Applied
    }

    pub(crate) fn mark_interrupt_persistence_failed(
        &self,
        expected_generation: &SamplingGenerationId,
    ) -> TerminalWakeResult {
        let _decision = self
            .terminal_decision_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.wake_generation_matches(expected_generation) {
            return TerminalWakeResult::Stale;
        }
        if self.interrupt_fence_state() == InterruptFenceState::PendingPersistence {
            self.interrupt_fence_state.store(
                InterruptFenceState::PersistenceFailed as u8,
                Ordering::Release,
            );
        }
        self.interrupt_resolution_notify.notify_waiters();
        TerminalWakeResult::Applied
    }

    pub(crate) fn interrupt_persistence_failed(&self) -> bool {
        self.interrupt_fence_state() == InterruptFenceState::PersistenceFailed
    }

    pub(crate) async fn wait_for_interrupt_resolution(
        &self,
        expected_generation: &SamplingGenerationId,
    ) -> TerminalWakeResult {
        let _waiter = TerminalWaiterGuard::new(&self.interrupt_resolution_waiters);
        loop {
            let notified = self.interrupt_resolution_notify.notified();
            if !self.wake_generation_matches(expected_generation) {
                return TerminalWakeResult::Stale;
            }
            if self.interrupt_fence_state() != InterruptFenceState::PendingPersistence {
                return TerminalWakeResult::Applied;
            }
            notified.await;
        }
    }

    fn release_interrupt_fence(&self) {
        let _decision = self
            .terminal_decision_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.interrupt_fence_state
            .store(InterruptFenceState::Open as u8, Ordering::Release);
        self.interrupt_resolution_notify.notify_waiters();
    }

    fn interrupt_fence_state(&self) -> InterruptFenceState {
        InterruptFenceState::from_u8(self.interrupt_fence_state.load(Ordering::Acquire))
    }

    pub(crate) async fn wait_completed(&self) {
        let _waiter = TerminalWaiterGuard::new(&self.completion_waiters);
        loop {
            let notified = self.completion_notify.notified();
            if self.interaction_released() {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_cleanup_completed(&self) {
        let _waiter = TerminalWaiterGuard::new(&self.cleanup_waiters);
        loop {
            let notified = self.cleanup_completion_notify.notified();
            if self.cleanup_completed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn request_panic_before_worker_cancellation(&self) {
        self.panic_before_worker_cancellation
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn panic_before_worker_cancellation_if_requested(&self) {
        if self
            .panic_before_worker_cancellation
            .swap(false, Ordering::AcqRel)
        {
            panic!("injected panic before worker cancellation");
        }
    }
}

pub(crate) struct TurnTerminalPermit {
    coordinator: Arc<TurnTerminalCoordinator>,
    cleanup_completed: bool,
}

impl TurnTerminalPermit {
    /// Claims the single best-effort analytics attempt for this coordinator.
    /// This marker is process-local and intentionally is not durable across restarts.
    pub(crate) fn try_claim_analytics_emission(&self) -> bool {
        self.coordinator
            .analytics_emission_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn mark_interaction_released(&self) {
        self.coordinator.mark_interaction_released();
    }

    pub(crate) fn complete_cleanup(mut self) {
        self.coordinator
            .interaction_released
            .store(true, Ordering::Release);
        self.coordinator.completion_notify.notify_waiters();
        self.coordinator
            .cleanup_completed
            .store(true, Ordering::Release);
        self.coordinator.cleanup_completion_notify.notify_waiters();
        self.coordinator.release_interrupt_fence();
        self.cleanup_completed = true;
    }
}

impl Drop for TurnTerminalPermit {
    fn drop(&mut self) {
        if !self.cleanup_completed {
            let _decision = self
                .coordinator
                .terminal_decision_gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.coordinator.claimed.store(false, Ordering::Release);
        }
    }
}

/// Mutable state for a single turn.
#[derive(Default)]
pub(crate) struct TurnState {
    pending_approvals: HashMap<String, oneshot::Sender<ReviewDecision>>,
    pending_request_permissions: HashMap<String, PendingRequestPermissions>,
    pending_user_input: HashMap<String, oneshot::Sender<RequestUserInputResponse>>,
    pending_elicitations: HashMap<(String, RequestId), oneshot::Sender<ElicitationResponse>>,
    pending_dynamic_tools: HashMap<String, oneshot::Sender<DynamicToolResponse>>,
    pub(crate) pending_input: TurnInputQueue,
    mailbox_delivery_phase: MailboxDeliveryPhase,
    granted_permissions_by_approval_scope_id: HashMap<String, UriAdditionalPermissionProfile>,
    strict_auto_review_enabled: bool,
    pub(crate) tool_calls: u64,
    pub(crate) has_memory_citation: bool,
    pub(crate) token_usage_at_turn_start: TokenUsage,
}

pub(crate) struct PendingRequestPermissions {
    pub(crate) tx_response: oneshot::Sender<RequestPermissionsResponse>,
    pub(crate) requested_permissions: RequestPermissionProfile,
    pub(crate) environment: TurnEnvironmentSelection,
    pub(crate) approval_scope_id: String,
}

impl TurnState {
    pub(crate) fn insert_pending_approval(
        &mut self,
        key: String,
        tx: oneshot::Sender<ReviewDecision>,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        self.pending_approvals.insert(key, tx)
    }

    pub(crate) fn remove_pending_approval(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<ReviewDecision>> {
        self.pending_approvals.remove(key)
    }

    pub(crate) fn clear_pending_waiters(&mut self) {
        self.pending_approvals.clear();
        self.pending_request_permissions.clear();
        self.pending_user_input.clear();
        self.pending_elicitations.clear();
        self.pending_dynamic_tools.clear();
    }

    pub(crate) fn insert_pending_request_permissions(
        &mut self,
        key: String,
        pending_request_permissions: PendingRequestPermissions,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions
            .insert(key, pending_request_permissions)
    }

    pub(crate) fn remove_pending_request_permissions(
        &mut self,
        key: &str,
    ) -> Option<PendingRequestPermissions> {
        self.pending_request_permissions.remove(key)
    }

    pub(crate) fn insert_pending_user_input(
        &mut self,
        key: String,
        tx: oneshot::Sender<RequestUserInputResponse>,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.insert(key, tx)
    }

    pub(crate) fn remove_pending_user_input(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<RequestUserInputResponse>> {
        self.pending_user_input.remove(key)
    }

    pub(crate) fn insert_pending_elicitation(
        &mut self,
        server_name: String,
        request_id: RequestId,
        tx: oneshot::Sender<ElicitationResponse>,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .insert((server_name, request_id), tx)
    }

    pub(crate) fn remove_pending_elicitation(
        &mut self,
        server_name: &str,
        request_id: &RequestId,
    ) -> Option<oneshot::Sender<ElicitationResponse>> {
        self.pending_elicitations
            .remove(&(server_name.to_string(), request_id.clone()))
    }

    pub(crate) fn try_insert_pending_dynamic_tool(
        &mut self,
        key: String,
        tx: oneshot::Sender<DynamicToolResponse>,
    ) -> Result<(), oneshot::Sender<DynamicToolResponse>> {
        match self.pending_dynamic_tools.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(tx);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(_) => Err(tx),
        }
    }

    pub(crate) fn remove_pending_dynamic_tool(
        &mut self,
        key: &str,
    ) -> Option<oneshot::Sender<DynamicToolResponse>> {
        self.pending_dynamic_tools.remove(key)
    }

    pub(crate) fn accept_mailbox_delivery_for_current_turn(&mut self) {
        self.set_mailbox_delivery_phase(MailboxDeliveryPhase::CurrentTurn);
    }

    pub(crate) fn accepts_mailbox_delivery_for_current_turn(&self) -> bool {
        self.mailbox_delivery_phase == MailboxDeliveryPhase::CurrentTurn
    }

    pub(crate) fn set_mailbox_delivery_phase(&mut self, phase: MailboxDeliveryPhase) {
        self.mailbox_delivery_phase = phase;
    }

    pub(crate) fn record_granted_permissions(
        &mut self,
        approval_scope_id: &str,
        permissions: UriAdditionalPermissionProfile,
    ) {
        let granted_permissions = merge_uri_permission_profiles(
            self.granted_permissions_by_approval_scope_id
                .get(approval_scope_id),
            Some(&permissions),
        );
        if let Some(granted_permissions) = granted_permissions {
            self.granted_permissions_by_approval_scope_id
                .insert(approval_scope_id.to_string(), granted_permissions);
        }
    }

    pub(crate) fn granted_permissions(
        &self,
        approval_scope_id: &str,
    ) -> Option<UriAdditionalPermissionProfile> {
        self.granted_permissions_by_approval_scope_id
            .get(approval_scope_id)
            .cloned()
    }

    pub(crate) fn enable_strict_auto_review(&mut self) {
        self.strict_auto_review_enabled = true;
    }

    pub(crate) fn strict_auto_review_enabled(&self) -> bool {
        self.strict_auto_review_enabled
    }
}
