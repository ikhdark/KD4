use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use crate::AgentGate;
use crate::AgentReceipt;
use crate::AgentTask;
use crate::AgentTaskBinding;
use crate::AgentTaskBindingDraft;
use crate::Assignment;
use crate::AssignmentDraft;
use crate::AssignmentId;
use crate::Attempt;
use crate::AttemptAmendment;
use crate::AttemptId;
use crate::AttributionConfidence;
use crate::GateKind;
use crate::GateStatus;
use crate::MutationEventId;
use crate::MutationEvidence;
use crate::MutationSnapshotChunk;
use crate::MutationSnapshotVersion;
use crate::ObservationKind;
use crate::ReceiptDraft;
use crate::RuntimeObservation;
use crate::StoreError;
use crate::StoreResult;
use crate::TaskActor;
use crate::ValidationCall;
use crate::WakeEventId;
use crate::WakeRead;

pub type TaskStoreFuture<'a, T> = Pin<Box<dyn Future<Output = StoreResult<T>> + Send + 'a>>;

/// Persistence contract used by the core coordination layer.
pub trait AgentTaskStore: Send + Sync {
    fn create_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
    ) -> TaskStoreFuture<'a, (Assignment, Attempt)>;

    fn get_agent_task(
        &self,
        assignment_id: AssignmentId,
        observation_limit: Option<usize>,
    ) -> TaskStoreFuture<'_, AgentTask>;

    fn bind_agent_task(
        &self,
        binding: AgentTaskBindingDraft,
    ) -> TaskStoreFuture<'_, AgentTaskBinding>;

    /// Removes only the runtime binding for a sealed failed-start task. The assignment, attempts,
    /// receipts, observations, and mutation evidence remain durable.
    fn remove_agent_task_binding(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            let _ = (actor, assignment_id);
            Err(StoreError::InvalidAssignment(
                "agent task binding removal is not supported by this store".to_string(),
            ))
        })
    }

    fn get_agent_task_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, Option<AgentTaskBinding>>;

    fn list_agent_task_bindings(
        &self,
        root_session_id: String,
        limit: Option<usize>,
    ) -> TaskStoreFuture<'_, Vec<AgentTaskBinding>>;

    fn append_observation(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        summary: String,
        call_id: Option<String>,
    ) -> TaskStoreFuture<'_, RuntimeObservation>;

    fn record_validation_call(&self, call: ValidationCall) -> TaskStoreFuture<'_, ()>;

    fn submit_agent_receipt(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
    ) -> TaskStoreFuture<'_, AgentReceipt>;

    /// Seals a successful receipt while atomically retaining the assignment behind a
    /// risk-derived cold-review gate. Implementations must create the risk/review gates in the
    /// same transaction as the receipt so an invalid receipt cannot leave an orphaned gate and a
    /// valid high-risk receipt cannot briefly release its write claim.
    fn submit_agent_receipt_with_review(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
        review_reason: String,
    ) -> TaskStoreFuture<'_, AgentReceipt>;

    fn amend_agent_task(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        amendment: AttemptAmendment,
    ) -> TaskStoreFuture<'_, Attempt>;

    fn abandon_agent_task(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentReceipt>;

    fn set_agent_gate(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        kind: GateKind,
        status: GateStatus,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate>;

    fn waive_agent_gate(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        kind: GateKind,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate>;

    fn read_wake_events(
        &self,
        root_session_id: String,
        after_event_id: Option<WakeEventId>,
    ) -> TaskStoreFuture<'_, WakeRead>;

    fn begin_mutation<'a>(
        &'a self,
        attempt_id: AttemptId,
        repo_root: &'a Path,
        path: String,
        confidence: AttributionConfidence,
    ) -> TaskStoreFuture<'a, MutationEventId>;

    fn finalize_mutation<'a>(
        &'a self,
        attempt_id: AttemptId,
        repo_root: &'a Path,
        path: String,
    ) -> TaskStoreFuture<'a, MutationEvidence>;

    /// Finalizes every mutation that was started for the active attempt, using the immutable
    /// repository binding captured when its assignment was created.
    fn finalize_pending_mutations(
        &self,
        attempt_id: AttemptId,
    ) -> TaskStoreFuture<'_, Vec<MutationEvidence>>;

    fn list_mutation_evidence(
        &self,
        attempt_id: AttemptId,
        limit: Option<usize>,
    ) -> TaskStoreFuture<'_, Vec<MutationEvidence>>;

    fn read_mutation_snapshot(
        &self,
        attempt_id: AttemptId,
        path: String,
        version: MutationSnapshotVersion,
        offset: u64,
        max_bytes: Option<usize>,
    ) -> TaskStoreFuture<'_, MutationSnapshotChunk>;
}
