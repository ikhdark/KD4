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
use crate::QuiescenceStatus;
use crate::ReceiptDraft;
use crate::RuntimeObservation;
use crate::StoreError;
use crate::StoreResult;
use crate::TaskActor;
use crate::ValidationCall;
use crate::WakeEventId;
use crate::WakeRead;
use crate::WorkspaceActorRegistration;
use crate::WorkspaceManifestEntry;
use crate::WorkspaceMutationLease;
use crate::WorkspaceMutationRequest;
use crate::WorkspaceMutationResult;
use crate::WorkspaceRevision;
use chrono::DateTime;
use chrono::Utc;

pub type TaskStoreFuture<'a, T> = Pin<Box<dyn Future<Output = StoreResult<T>> + Send + 'a>>;

/// Persistence contract used by the core coordination layer.
pub trait AgentTaskStore: Send + Sync {
    fn create_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
    ) -> TaskStoreFuture<'a, (Assignment, Attempt)>;

    /// Atomically attaches the one immutable, canonical TaskCapsule bootstrap snapshot.
    fn attach_task_capsule(
        &self,
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
        canonical_payload: String,
    ) -> TaskStoreFuture<'_, Assignment>;

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

    /// Renews only the exact, currently bound, active typed actor. Missing, sealed,
    /// superseded, terminal, or mismatched bindings return `false` without revival.
    fn heartbeat_typed_workspace_actor(
        &self,
        binding: AgentTaskBinding,
    ) -> TaskStoreFuture<'_, bool>;

    fn append_observation(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        summary: String,
        call_id: Option<String>,
    ) -> TaskStoreFuture<'_, RuntimeObservation>;

    fn record_validation_call(&self, call: ValidationCall) -> TaskStoreFuture<'_, ()>;

    fn get_validation_call(&self, call_id: String) -> TaskStoreFuture<'_, Option<ValidationCall>>;

    fn heartbeat_validation_call(
        &self,
        call_id: String,
        lease_expires_at: DateTime<Utc>,
    ) -> TaskStoreFuture<'_, bool>;

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

    fn reserve_stalled_nudge(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: DateTime<Utc>,
    ) -> TaskStoreFuture<'_, bool>;

    fn release_stalled_nudge(&self, assignment_id: AssignmentId) -> TaskStoreFuture<'_, bool>;

    fn capture_workspace_revision<'a>(
        &'a self,
        repo_root: &'a Path,
        paths: Vec<String>,
    ) -> TaskStoreFuture<'a, WorkspaceRevision>;

    fn record_supporting_read<'a>(
        &'a self,
        repo_root: &'a Path,
        actor_id: String,
        paths: Vec<String>,
    ) -> TaskStoreFuture<'a, WorkspaceRevision>;

    fn record_supporting_read_entries<'a>(
        &'a self,
        repo_root: &'a Path,
        actor_id: String,
        entries: Vec<WorkspaceManifestEntry>,
    ) -> TaskStoreFuture<'a, WorkspaceRevision>;

    fn supporting_read_manifest<'a>(
        &'a self,
        repo_root: &'a Path,
        actor_id: String,
        paths: Vec<String>,
    ) -> TaskStoreFuture<'a, Vec<crate::WorkspaceManifestEntry>>;

    fn read_workspace_events<'a>(
        &'a self,
        repo_root: &'a Path,
        after_epoch: u64,
    ) -> TaskStoreFuture<'a, Vec<crate::WorkspaceEvent>>;

    fn register_workspace_actor<'a>(
        &'a self,
        repo_root: &'a Path,
        registration: WorkspaceActorRegistration,
    ) -> TaskStoreFuture<'a, ()>;

    fn begin_workspace_mutation<'a>(
        &'a self,
        repo_root: &'a Path,
        request: WorkspaceMutationRequest,
    ) -> TaskStoreFuture<'a, WorkspaceMutationLease>;

    fn heartbeat_workspace_mutation<'a>(
        &'a self,
        repo_root: &'a Path,
        lease_id: String,
        actor_id: String,
    ) -> TaskStoreFuture<'a, bool>;

    fn finish_workspace_mutation<'a>(
        &'a self,
        repo_root: &'a Path,
        lease: WorkspaceMutationLease,
    ) -> TaskStoreFuture<'a, WorkspaceMutationResult>;

    fn begin_workspace_finalization<'a>(
        &'a self,
        repo_root: &'a Path,
        root_session_id: String,
    ) -> TaskStoreFuture<'a, crate::WorkspaceFinalizationFence>;

    /// Atomically seals an active, unexpired finalization fence for terminal dispatch.
    /// Successful sealing refreshes the bounded lease and returns the updated fence.
    fn seal_workspace_finalization_dispatch<'a>(
        &'a self,
        repo_root: &'a Path,
        fence: crate::WorkspaceFinalizationFence,
    ) -> TaskStoreFuture<'a, crate::WorkspaceFinalizationFence>;

    fn heartbeat_workspace_finalization<'a>(
        &'a self,
        repo_root: &'a Path,
        fence_id: String,
        root_session_id: String,
    ) -> TaskStoreFuture<'a, bool>;

    fn release_workspace_finalization<'a>(
        &'a self,
        repo_root: &'a Path,
        fence: crate::WorkspaceFinalizationFence,
    ) -> TaskStoreFuture<'a, ()>;

    fn assert_workspace_unclaimed<'a>(
        &'a self,
        repo_root: &'a Path,
        actor_attempt_id: Option<AttemptId>,
    ) -> TaskStoreFuture<'a, ()>;

    fn check_quiescence(&self, root_session_id: String) -> TaskStoreFuture<'_, QuiescenceStatus>;

    /// Read the already-reconciled quiescence state without expiring leases,
    /// releasing claims, or refreshing validation evidence.
    fn inspect_quiescence(&self, root_session_id: String) -> TaskStoreFuture<'_, QuiescenceStatus>;

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
