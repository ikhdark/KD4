use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::ThreadId;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::plan_tool::ValidationRouteOrdering;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TurnTimingTerminalization;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use codex_protocol::validation::ValidationResult;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha1::Digest;
use sha1::Sha1;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::debug;
use tracing::warn;

use crate::terminal_event_fingerprint;
use crate::turn_diff_tracker::CommandMutation;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 12;
const FROZEN_TASK_EVIDENCE_V11_SCHEMA_VERSION: u32 = 11;
const FROZEN_TASK_EVIDENCE_V10_SCHEMA_VERSION: u32 = 10;
const FROZEN_TASK_EVIDENCE_V9_SCHEMA_VERSION: u32 = 9;
const FROZEN_TASK_EVIDENCE_V8_SCHEMA_VERSION: u32 = 8;
const FROZEN_TASK_EVIDENCE_V7_SCHEMA_VERSION: u32 = 7;
const FROZEN_TASK_EVIDENCE_V6_SCHEMA_VERSION: u32 = 6;
const FROZEN_TASK_EVIDENCE_V5_SCHEMA_VERSION: u32 = 5;
const FROZEN_TASK_EVIDENCE_V4_SCHEMA_VERSION: u32 = 4;
pub(crate) const SOURCE_CLASSIFICATION_CONTRACT_VERSION: &str = "source-local-v2";
pub(crate) const RELATIONSHIP_RESOLVER_CONTRACT_VERSION: &str = "relationship-v1";
const TASK_EVIDENCE_COMPLETION_MODEL_VERSION: u32 = 3;
const FILE_HASH_CHUNK_SIZE: usize = 64 * 1024;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_EXTERNAL_EVIDENCE_RECEIPTS: usize = 256;
const MAX_COMPLETION_REVIEW_RECEIPTS: usize = 256;
const MAX_ATTRIBUTED_WORKSPACE_EVENTS: usize = 256;
const MAX_TERMINALIZATION_RECEIPTS: usize = 64;
const MAX_LOCKED_USER_DECISIONS: usize = 256;
const MAX_PLANNING_AUDIT_ENTRIES: usize = 512;
const MAX_OUTSIDE_PLAN_ACTIONS: usize = 256;
const MAX_COMPLETION_CHECKPOINT_HUNK_BYTES: usize = 16 * 1024;
const COMPACTION_TASK_STATE_MAX_TOKENS: usize = 2_400;
const EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES: usize = 16 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES: usize = 8 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_HEADER: &str =
    "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1\n";
const USER_SOURCE_LEDGER_CANONICAL_FORMAT: &str = "KD4_USER_SOURCE_LEDGER_CANONICAL_V1";
const REQUIREMENT_MANIFEST_CANONICAL_FORMAT: &str = "KD4_REQUIREMENT_MANIFEST_CANONICAL_V1";
const IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT: &str = "KD4_IMPLEMENTATION_IDENTITY_CANONICAL_V1";
const DOSSIER_SNAPSHOT_CANONICAL_FORMAT: &str = "KD4_DOSSIER_SNAPSHOT_CANONICAL_V1";
const DESKTOP_INSTALL_EVIDENCE_CANONICAL_FORMAT: &str = "KD4_DESKTOP_INSTALL_EVIDENCE_CANONICAL_V1";
const DESKTOP_ACTIVATION_CHALLENGE_CANONICAL_FORMAT: &str =
    "KD4_DESKTOP_ACTIVATION_CHALLENGE_CANONICAL_V1";
const DESKTOP_ACTIVATION_RECEIPT_CANONICAL_FORMAT: &str =
    "KD4_DESKTOP_ACTIVATION_RECEIPT_CANONICAL_V1";
const DESKTOP_INSTALL_EVIDENCE_SCHEMA_VERSION: u32 = 1;
const DESKTOP_ACTIVATION_CHALLENGE_TTL_SECONDS: i64 = 120;
const REPAIR_INSTRUCTION_CANONICAL_FORMAT: &str = "KD4_REPAIR_INSTRUCTION_CANONICAL_V1";
const REPAIR_BASELINE_CANONICAL_FORMAT: &str = "KD4_REPAIR_BASELINE_CANONICAL_V1";
const REPAIR_DELTA_CANONICAL_FORMAT: &str = "KD4_REPAIR_DELTA_CANONICAL_V1";
const REREVIEW_AUDIT_CANONICAL_FORMAT: &str = "KD4_REREVIEW_AUDIT_CANONICAL_V1";
const REPAIR_PATH_GRAMMAR_VERSION: u32 = 1;
const REPAIR_RECURSIVE_WILDCARD_MAX_SEGMENTS: usize = 8;
pub(crate) const COMPLETION_REVIEW_LENSES: [&str; 8] = [
    "requirements_and_behavioral_compatibility",
    "lifecycle_and_concurrency",
    "persistence_filesystem_safety_rollback_and_atomicity",
    "schema_protocol_and_generated_representations",
    "security_and_trust_boundaries",
    "platform_configuration_packaging_and_installation",
    "pipeline_cache_snapshot_and_artifact_identity",
    "validation_quality_and_changed_test_oracle_integrity",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskEvidenceMode {
    Disabled,
    EvidenceOnly,
    Kd4Completion,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanningTier {
    #[default]
    Focused,
    Medium,
    Complex,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValidationDisposition {
    Executable,
    UnresolvedDiscoverable,
    UnavailableBlocked,
    #[default]
    NotRequired,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultProvenance {
    DirectFileRead,
    SearchHit,
    GeneratedSummary,
    CachedObservation,
    InferredRelationship,
    TestResult,
    /// Compatibility value for facts persisted before provenance was required.
    #[default]
    Unverified,
}

impl ResultProvenance {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DirectFileRead => "direct_file_read",
            Self::SearchHit => "search_hit",
            Self::GeneratedSummary => "generated_summary",
            Self::CachedObservation => "cached_observation",
            Self::InferredRelationship => "inferred_relationship",
            Self::TestResult => "test_result",
            Self::Unverified => "unverified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanningFactInput {
    pub(crate) id: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) provenance: ResultProvenance,
    #[serde(default)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) depends_on_paths: Vec<String>,
    #[serde(default = "planning_fact_dependencies_current_default")]
    pub(crate) dependencies_current: bool,
}

const fn planning_fact_dependencies_current_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReasonedPlanningRemoval {
    pub(crate) id: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MutationObligationInput {
    pub(crate) id: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExternalValidationRouteInput {
    pub(crate) server_name: String,
    pub(crate) tool_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanStepEvidenceInput {
    pub(crate) step_id: String,
    #[serde(default)]
    pub(crate) source_owner: Option<String>,
    #[serde(default)]
    pub(crate) implementation_surfaces: Vec<String>,
    #[serde(default)]
    pub(crate) mutation_obligations: Vec<MutationObligationInput>,
    #[serde(default)]
    pub(crate) validation_disposition: Option<ValidationDisposition>,
    #[serde(default)]
    pub(crate) external_validation_route: Option<ExternalValidationRouteInput>,
}

/// Core-only planning input. The public PlanUpdate event continues to contain
/// only the materialized `UpdatePlanArgs` projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanningUpdateInput {
    #[serde(default)]
    pub(crate) explanation: Option<String>,
    #[serde(default)]
    pub(crate) tier: Option<PlanningTier>,
    #[serde(default)]
    pub(crate) facts: Vec<PlanningFactInput>,
    #[serde(default)]
    pub(crate) removed_facts: Vec<ReasonedPlanningRemoval>,
    #[serde(default)]
    pub(crate) removed_steps: Vec<ReasonedPlanningRemoval>,
    #[serde(default)]
    pub(crate) source_owner: Option<String>,
    #[serde(default)]
    pub(crate) implementation_surfaces: Vec<String>,
    #[serde(default)]
    pub(crate) acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub(crate) mutation_obligations: Vec<MutationObligationInput>,
    #[serde(default)]
    pub(crate) validation_disposition: Option<ValidationDisposition>,
    #[serde(default)]
    pub(crate) validation_route: Option<ValidationRoute>,
    #[serde(default)]
    pub(crate) external_validation_route: Option<ExternalValidationRouteInput>,
    #[serde(default)]
    pub(crate) step_evidence: Vec<PlanStepEvidenceInput>,
    #[serde(default)]
    pub(crate) plan: Vec<PlanItemArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanUpdateEffect {
    Initial,
    StructuralRevision,
    StatusOnly,
    NoOp,
}

impl PlanUpdateEffect {
    pub(crate) fn requests_generation(self) -> bool {
        matches!(self, Self::Initial | Self::StructuralRevision)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::StructuralRevision => "structural_revision",
            Self::StatusOnly => "status_only",
            Self::NoOp => "no_op",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanUpdateOutcome {
    pub(crate) public_update: UpdatePlanArgs,
    pub(crate) effect: PlanUpdateEffect,
    /// Authoritative durable-plan proof for pre-edit final-validation admission.
    /// `None` means the task-evidence mode cannot prove either state.
    pub(crate) unfinished_mutation_obligation: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FinalProofStateV1 {
    #[serde(default)]
    pub(crate) basis: Option<CompletionCandidateBasisV1>,
    #[serde(default)]
    pub(crate) validation_plan: Option<ValidationPlanV1>,
    #[serde(default)]
    pub(crate) candidate: Option<CompletionCandidateV1>,
    #[serde(default)]
    pub(crate) diff_snapshot: Option<CandidateDiffSnapshotV1>,
    #[serde(default)]
    pub(crate) proof_observations: Vec<FinalProofObservationV1>,
    #[serde(default)]
    pub(crate) failure_fingerprint: Option<CompletionFailureFingerprintV1>,
    #[serde(default)]
    pub(crate) terminal_decision: Option<TaskCompletionGate>,
    #[serde(default)]
    pub(crate) reasons: Vec<CompletionReasonV1>,
    #[serde(default)]
    pub(crate) checkpoint: Option<CompletionCheckpointV1>,
    #[serde(default)]
    pub(crate) reviewer_infrastructure_memo: Option<ReviewerInfrastructureMemoV1>,
    #[serde(default)]
    pub(crate) finalization_memo: Option<CompletionFinalizationMemoV1>,
    #[serde(default)]
    pub(crate) repair_count_by_lineage: BTreeMap<String, u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionCandidateBasisV1 {
    pub(crate) basis_id: String,
    pub(crate) implementation_identity: String,
    pub(crate) source_identity: String,
    pub(crate) requirement_identity: String,
    #[serde(default)]
    pub(crate) task_evidence_epoch: u64,
    #[serde(default)]
    pub(crate) host_mutation_revision: u64,
    pub(crate) workspace_epoch: u64,
    pub(crate) workspace_manifest_identity: String,
    pub(crate) environment_identity: String,
    pub(crate) toolchain_identity: String,
    pub(crate) features_identity: String,
    pub(crate) configuration_identity: String,
    #[serde(default)]
    pub(crate) child_gate_state: Vec<String>,
    pub(crate) reviewer_configuration_identity: String,
    pub(crate) canonical_diff_identity: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ValidationPlanV1 {
    pub(crate) plan_id: String,
    pub(crate) basis_id: String,
    #[serde(default)]
    pub(crate) steps: Vec<ValidationPlanStepV1>,
    pub(crate) ambiguous_or_unmappable: bool,
    pub(crate) resolution_generation_used: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ValidationPlanStepV1 {
    pub(crate) step_id: String,
    pub(crate) obligation_id: String,
    #[serde(default)]
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) covered_paths: Vec<String>,
    #[serde(default)]
    pub(crate) covered_contracts: Vec<String>,
    pub(crate) timeout_ms: u64,
    pub(crate) semantic_timeout: bool,
    pub(crate) batch_group: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionCandidateV1 {
    pub(crate) candidate_id: String,
    pub(crate) basis_id: String,
    pub(crate) validation_plan_id: String,
    pub(crate) lineage_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CandidateDiffSnapshotV1 {
    pub(crate) candidate_id: String,
    pub(crate) diff_identity: String,
    #[serde(default)]
    pub(crate) head_identity: Option<String>,
    #[serde(default)]
    pub(crate) index_identity: Option<String>,
    #[serde(default)]
    pub(crate) worktree_identity: Option<String>,
    #[serde(default)]
    pub(crate) changed_paths: Vec<String>,
    #[serde(default)]
    pub(crate) bounded_hunks: String,
    pub(crate) raw_artifact_digest: String,
    #[serde(default)]
    pub(crate) raw_artifact_ref: Option<String>,
    pub(crate) workspace_epoch: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FinalProofObservationV1 {
    pub(crate) candidate_id: String,
    pub(crate) plan_step_id: String,
    pub(crate) obligation_id: String,
    pub(crate) successful: bool,
    pub(crate) complete_identity: bool,
    pub(crate) invocation_identity: String,
    pub(crate) coverage_identity: String,
    pub(crate) retained_output_digest: String,
    #[serde(default)]
    pub(crate) retained_output_ref: Option<String>,
    pub(crate) evidence_revision: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionFailureFingerprintV1 {
    pub(crate) fingerprint: String,
    pub(crate) candidate_id: String,
    pub(crate) correctness_evidence_revision: u64,
    #[serde(default)]
    pub(crate) missing_or_failed_obligation_ids: Vec<String>,
    #[serde(default)]
    pub(crate) child_gate_state: Vec<String>,
    #[serde(default)]
    pub(crate) reviewer_state: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReasonV1 {
    pub(crate) reason_code: String,
    #[serde(default)]
    pub(crate) obligation_ids: Vec<String>,
    #[serde(default)]
    pub(crate) path_ids: Vec<String>,
    #[serde(default)]
    pub(crate) contract_ids: Vec<String>,
    pub(crate) first_epoch: u64,
    pub(crate) last_epoch: u64,
    pub(crate) occurrence_count: u64,
    pub(crate) latest_occurrence: String,
    #[serde(default)]
    pub(crate) evidence_ref: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionCheckpointV1 {
    pub(crate) checkpoint_id: String,
    pub(crate) candidate_id: String,
    pub(crate) basis_id: String,
    pub(crate) validation_plan_id: String,
    pub(crate) diff_identity: String,
    #[serde(default)]
    pub(crate) requirements: Vec<CheckpointRequirementV1>,
    #[serde(default)]
    pub(crate) affected_surfaces: Vec<String>,
    #[serde(default)]
    pub(crate) changed_paths: Vec<String>,
    #[serde(default)]
    pub(crate) bounded_hunks: String,
    #[serde(default)]
    pub(crate) proof_receipts: Vec<CheckpointProofReceiptV1>,
    #[serde(default)]
    pub(crate) unresolved_blockers: Vec<String>,
    #[serde(default)]
    pub(crate) unresolved_risks: Vec<String>,
    #[serde(default)]
    pub(crate) child_gate_state: Vec<String>,
    #[serde(default)]
    pub(crate) evidence_artifact_references: Vec<String>,
    pub(crate) estimated_tokens: u64,
}

impl CompletionCheckpointV1 {
    pub(crate) fn canonical_payload(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

fn completion_recovery_identity(
    basis: &CompletionCandidateBasisV1,
    turn_id: &str,
    terminal_hooks_completed: bool,
    mutation_quiescent: bool,
) -> CompletionRecoveryIdentityV1 {
    CompletionRecoveryIdentityV1 {
        implementation_identity: basis.implementation_identity.clone(),
        evidence_identity: canonical_hash(
            "KD4_COMPLETION_RECOVERY_EVIDENCE_V1",
            &serde_json::json!({
                "source": basis.source_identity,
                "requirements": basis.requirement_identity,
                "task_evidence_epoch": basis.task_evidence_epoch,
                "host_mutation_revision": basis.host_mutation_revision,
            }),
        ),
        workspace_identity: canonical_hash(
            "KD4_COMPLETION_RECOVERY_WORKSPACE_V1",
            &serde_json::json!({
                "epoch": basis.workspace_epoch,
                "manifest": basis.workspace_manifest_identity,
            }),
        ),
        diff_identity: basis.canonical_diff_identity.clone(),
        environment_identity: basis.environment_identity.clone(),
        toolchain_identity: basis.toolchain_identity.clone(),
        configuration_identity: canonical_hash(
            "KD4_COMPLETION_RECOVERY_CONFIGURATION_V1",
            &serde_json::json!({
                "features": basis.features_identity,
                "configuration": basis.configuration_identity,
            }),
        ),
        reviewer_identity: basis.reviewer_configuration_identity.clone(),
        child_gate_identity: canonical_hash(
            "KD4_COMPLETION_RECOVERY_CHILD_GATE_V1",
            &serde_json::json!(&basis.child_gate_state),
        ),
        terminal_hook_identity: canonical_hash(
            "KD4_COMPLETION_RECOVERY_TERMINAL_HOOK_V1",
            &serde_json::json!({
                "turn_id": turn_id,
                "terminal_hooks_completed": terminal_hooks_completed,
                "mutation_quiescent": mutation_quiescent,
            }),
        ),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckpointRequirementV1 {
    pub(crate) requirement_id: String,
    pub(crate) source_id: String,
    pub(crate) exact_text: String,
    pub(crate) status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckpointProofReceiptV1 {
    pub(crate) obligation_id: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) evidence_ref: Option<String>,
    pub(crate) evidence_digest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewerInfrastructureMemoV1 {
    pub(crate) identity: String,
    pub(crate) candidate_id: String,
    pub(crate) dossier_id: String,
    pub(crate) reviewer_configuration_identity: String,
    pub(crate) infrastructure_condition_identity: String,
    pub(crate) outcome: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionFinalizationMemoV1 {
    pub(crate) identity: String,
    pub(crate) candidate_id: String,
    pub(crate) checkpoint_id: String,
    #[serde(default)]
    pub(crate) turn_id: Option<String>,
    #[serde(default)]
    pub(crate) terminal_hooks_completed: bool,
    #[serde(default)]
    pub(crate) mutation_quiescent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recovery_identity: Option<CompletionRecoveryIdentityV1>,
    pub(crate) final_message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionRecoveryIdentityV1 {
    pub(crate) implementation_identity: String,
    pub(crate) evidence_identity: String,
    pub(crate) workspace_identity: String,
    pub(crate) diff_identity: String,
    pub(crate) environment_identity: String,
    pub(crate) toolchain_identity: String,
    pub(crate) configuration_identity: String,
    pub(crate) reviewer_identity: String,
    pub(crate) child_gate_identity: String,
    pub(crate) terminal_hook_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionRecoveryIntentV1 {
    pub(crate) turn_id: String,
    pub(crate) memo_identity: String,
    pub(crate) final_message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FinalProofSealInputV1 {
    pub(crate) implementation_identity: String,
    pub(crate) source_identity: String,
    pub(crate) requirement_identity: String,
    pub(crate) workspace_epoch: u64,
    pub(crate) workspace_manifest_identity: String,
    pub(crate) environment_identity: String,
    pub(crate) toolchain_identity: String,
    pub(crate) features_identity: String,
    pub(crate) configuration_identity: String,
    pub(crate) child_gate_state: Vec<String>,
    pub(crate) reviewer_configuration_identity: String,
    pub(crate) diff_snapshot: CandidateDiffSnapshotV1,
    pub(crate) checkpoint_token_budget: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FinalProofIdentitySnapshotV1 {
    pub(crate) implementation_identity: String,
    pub(crate) source_identity: String,
    pub(crate) requirement_identity: String,
    pub(crate) task_evidence_epoch: u64,
    pub(crate) host_mutation_revision: u64,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) workspace_path_snapshot_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FinalProofSealResultV1 {
    Sealed {
        candidate: CompletionCandidateV1,
        validation_plan: ValidationPlanV1,
        checkpoint: Box<CompletionCheckpointV1>,
        telemetry: FinalProofSealTelemetryV1,
        gate: TaskCompletionGate,
    },
    Memoized {
        gate: TaskCompletionGate,
        checkpoint_tokens: u64,
        telemetry: FinalProofSealTelemetryV1,
    },
    PreflightFailed(TaskCompletionGate),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FinalProofSealTelemetryV1 {
    pub(crate) validation_launch_count: u32,
    pub(crate) validation_process_ns: u64,
    pub(crate) validation_aggregate_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalEvidenceCapture {
    Ignored,
    Stored,
    Warning(&'static str),
}

#[derive(Clone)]
pub(crate) struct TaskEvidenceLedger {
    mode: TaskEvidenceMode,
    codex_home: Option<PathBuf>,
    thread_id: Option<String>,
    evidence_path: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    document: Arc<Mutex<Option<TaskEvidenceDocument>>>,
    persistence_gate: Arc<Semaphore>,
    external_evidence_gate: Arc<Semaphore>,
    freshness_gate: Arc<Semaphore>,
    freshness_state: Arc<std::sync::Mutex<FreshnessState>>,
    last_persisted_revision: Arc<AtomicU64>,
    source_capture_failed: Arc<AtomicBool>,
    desktop_activation_gate: Arc<Semaphore>,
    desktop_activation_runtime: Arc<std::sync::Mutex<DesktopActivationRuntimeState>>,
    #[cfg(test)]
    persistence_test_control: Arc<std::sync::Mutex<Option<PersistenceTestControl>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrustedFileToken {
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    mtime: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
    #[cfg(windows)]
    volume_serial_number: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
    #[cfg(windows)]
    file_attributes: u32,
    #[cfg(windows)]
    last_write_time: i64,
    #[cfg(windows)]
    change_time: i64,
}

#[derive(Debug, Clone)]
struct CachedStrongHash {
    token: TrustedFileToken,
    sha1: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FreshnessManifest {
    state_signature: String,
    evidence_epoch: u64,
    requirements: Vec<GeneratedArtifactRequirement>,
    tracked: BTreeMap<String, FileHashSnapshot>,
    artifacts: BTreeMap<String, FileHashSnapshot>,
    artifact_paths: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct CompletionProof {
    manifest: FreshnessManifest,
    tokens: BTreeMap<PathBuf, TrustedFileToken>,
    all_reusable: bool,
}

#[derive(Debug)]
struct FreshnessScanResult {
    snapshots: BTreeMap<String, FileHashSnapshot>,
    tokens: BTreeMap<PathBuf, TrustedFileToken>,
    cache_updates: BTreeMap<PathBuf, CachedStrongHash>,
    cache_removals: BTreeSet<PathBuf>,
    all_reusable: bool,
}

#[derive(Debug)]
struct FreshnessPathObservation {
    snapshot: FileHashSnapshot,
    token: Option<TrustedFileToken>,
}

#[derive(Debug, Default)]
struct FreshnessState {
    cache: BTreeMap<PathBuf, CachedStrongHash>,
    completion_proof: Option<CompletionProof>,
    diagnostics: FreshnessDiagnostics,
    #[cfg(test)]
    before_next_scan: Option<(Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>)>,
    #[cfg(test)]
    force_untrusted_tokens: bool,
    #[cfg(test)]
    force_ambiguous_tokens: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FreshnessDiagnostics {
    pub(crate) scan_invocations: u64,
    pub(crate) files_strongly_hashed: u64,
    pub(crate) bytes_strongly_hashed: u64,
    pub(crate) strong_hashes_reused: u64,
    pub(crate) conservative_reruns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreshnessPurpose {
    Ordinary,
    CompletionFresh,
    CompletionRetry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildEvidenceProvenance {
    pub(crate) source_thread_id: String,
    pub(crate) source_agent_path: String,
}

type PersistenceWriteBarrierPair = (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistOutcome {
    Persisted,
    Superseded,
    Failed,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
struct PersistenceTestControl {
    before_next_write: Arc<std::sync::Mutex<Option<PersistenceWriteBarrierPair>>>,
    fail_writes: Arc<std::sync::atomic::AtomicBool>,
    supersede_writes: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskEvidenceDocument {
    schema_version: u32,
    #[serde(default)]
    revision: u64,
    thread_id: String,
    started_at: String,
    updated_at: String,
    start: TaskStartState,
    evidence_epoch: u64,
    last_mutation_at: Option<String>,
    #[serde(default)]
    planning: PlanningEvidenceState,
    plan: Vec<EvidencePlanStep>,
    active_step_id: Option<String>,
    /// An explicit model-authored implementation boundary. This is separate
    /// from edit-driven status promotion and is invalidated by relevant edits.
    #[serde(default)]
    batch_acknowledgement: Option<ImplementationBatchAcknowledgement>,
    edit_intents: Vec<EditIntent>,
    edit_receipts: Vec<EditReceipt>,
    command_receipts: Vec<CommandReceipt>,
    #[serde(default)]
    external_evidence: Vec<ExternalEvidenceReceipt>,
    #[serde(default)]
    completion_review_receipts: Vec<CompletionReviewAuditReceipt>,
    generated_artifact_requirements: Vec<GeneratedArtifactRequirement>,
    #[serde(default)]
    latest_generated_artifact_hashes: BTreeMap<String, FileHashSnapshot>,
    latest_file_hashes: BTreeMap<String, FileHashSnapshot>,
    risks: Vec<EvidenceRisk>,
    desktop_activation_receipt: Option<DesktopActivationReceipt>,
    #[serde(default = "initial_receipt_sequence")]
    next_edit_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_command_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_external_evidence_receipt_sequence: u64,
    #[serde(default)]
    host_mutation_revision: u64,
    #[serde(default)]
    completion_review_v2: Option<CompletionReviewLedgerV2>,
    #[serde(default, deserialize_with = "deserialize_source_classification_cache")]
    source_classification_cache: Vec<SourceClassificationCacheEntry>,
    #[serde(default)]
    terminalization_receipts: Vec<TerminalizationReceipt>,
    #[serde(default)]
    locked_user_decisions: Vec<LockedUserDecision>,
    #[serde(default)]
    final_proof: FinalProofStateV1,
    completion: Option<TaskCompletionGate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockedUserDecision {
    decision_id: String,
    call_id: String,
    turn_id: String,
    question_id: String,
    header: String,
    question: String,
    answers: Vec<String>,
    recorded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalDeliveryState {
    #[default]
    NotAttempted,
    Claimed,
    Delivered,
    DeliveryFailed,
}

impl TerminalDeliveryState {
    pub(crate) fn is_authoritative_claim(self) -> bool {
        !matches!(self, Self::NotAttempted)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalRecoveryState {
    #[default]
    None,
    Pending,
    Recovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthoritativeTerminalEventV1 {
    #[serde(default = "authoritative_terminal_event_version")]
    pub(crate) version: u32,
    pub(crate) terminal_identity: String,
    pub(crate) turn_id: String,
    pub(crate) event: EventMsg,
    pub(crate) fingerprint: String,
    pub(crate) semantic_outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) final_proof_identity: Option<String>,
}

const fn authoritative_terminal_event_version() -> u32 {
    1
}

impl AuthoritativeTerminalEventV1 {
    fn is_self_consistent(&self) -> bool {
        let event_turn_id = match &self.event {
            EventMsg::TurnComplete(event) => Some(event.turn_id.as_str()),
            EventMsg::TurnAborted(event) => event.turn_id.as_deref(),
            _ => None,
        };
        self.version == authoritative_terminal_event_version()
            && event_turn_id == Some(self.turn_id.as_str())
            && terminal_event_fingerprint(&self.event).as_deref() == Some(self.fingerprint.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TerminalizationReceipt {
    terminal_identity: String,
    durable_outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authoritative_event: Option<AuthoritativeTerminalEventV1>,
    delivery_state: TerminalDeliveryState,
    #[serde(default)]
    app_server_acknowledged: bool,
    #[serde(default)]
    runtime_status_converged: bool,
    #[serde(default)]
    rollout_mirrored: bool,
    #[serde(default)]
    parent_notification_completed: bool,
    #[serde(default)]
    post_terminal_cleanup_completed: bool,
    active_turn_detached: bool,
    terminal_interaction_released: bool,
    #[serde(default)]
    deadline_exhausted_phase: Option<String>,
    mutation_quiescent: bool,
    durable_success_established: bool,
    #[serde(default)]
    retained_ownership: Vec<String>,
    #[serde(default)]
    recovery_state: TerminalRecoveryState,
    #[serde(default)]
    phase_timings_ns: BTreeMap<String, u64>,
    /// Final schema-v10 terminal timing receipt, populated only by the
    /// post-cleanup non-blocking persistence pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminalization: Option<TurnTimingTerminalization>,
    recorded_at: String,
    updated_at: String,
}

impl TerminalizationReceipt {
    fn validated_authoritative_event(&self) -> Option<&AuthoritativeTerminalEventV1> {
        self.authoritative_event.as_ref().filter(|event| {
            event.is_self_consistent() && self.durable_outcome == event.semantic_outcome
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TerminalDecisionClaim {
    pub(crate) authoritative_event: AuthoritativeTerminalEventV1,
    pub(crate) deadline_exhausted_phase: Option<String>,
    pub(crate) mutation_quiescent: bool,
    pub(crate) durable_success_established: bool,
    pub(crate) retained_ownership: Vec<String>,
    pub(crate) phase_timings_ns: BTreeMap<String, u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct TerminalInteractionUpdate {
    pub(crate) terminal_identity: String,
    pub(crate) delivery_state: TerminalDeliveryState,
    pub(crate) app_server_acknowledged: bool,
    pub(crate) runtime_status_converged: bool,
    pub(crate) rollout_mirrored: bool,
    pub(crate) parent_notification_completed: bool,
    pub(crate) post_terminal_cleanup_completed: bool,
    pub(crate) active_turn_detached: bool,
    pub(crate) terminal_interaction_released: bool,
    pub(crate) recovery_state: TerminalRecoveryState,
    pub(crate) phase_timings_ns: BTreeMap<String, u64>,
    pub(crate) terminalization: Option<TurnTimingTerminalization>,
}

#[derive(Debug, Clone)]
pub(crate) struct TerminalizationReceiptSnapshot {
    pub(crate) terminal_identity: String,
    pub(crate) terminalization: TurnTimingTerminalization,
    pub(crate) delivery_state: TerminalDeliveryState,
    pub(crate) active_turn_detached: bool,
    pub(crate) terminal_interaction_released: bool,
    pub(crate) recovery_state: TerminalRecoveryState,
    pub(crate) deadline_exhausted_phase: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum TerminalClaimResult {
    Claimed(AuthoritativeTerminalEventV1),
    AlreadyClaimed(AuthoritativeTerminalEventV1),
    Conflict {
        authoritative: Option<AuthoritativeTerminalEventV1>,
        candidate_fingerprint: String,
    },
    Failed,
}

#[derive(Debug, Clone)]
enum TerminalClaimMutation {
    Inserted,
    Existing(AuthoritativeTerminalEventV1),
    Conflict(Option<AuthoritativeTerminalEventV1>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionReviewLedgerV2 {
    root_task_id: String,
    completion_epoch: u64,
    manifest_revision: u64,
    next_source_ordinal: u64,
    source_records: BTreeMap<String, UserSourceRecord>,
    mapping_revisions: Vec<SourceMappingRevision>,
    manifest_snapshots: Vec<RequirementManifestSnapshot>,
    active_review_cycle: Option<CompletionReviewCycle>,
    review_risk: CompletionReviewRisk,
    receipts: Vec<CompletionReviewReceiptV2>,
    next_review_sequence: u64,
    last_terminal_closure: Option<String>,
    #[serde(default)]
    last_workspace_event_epoch: u64,
    #[serde(default)]
    workspace_event_baseline_epoch: u64,
    #[serde(default)]
    typed_assignment_baseline: BTreeSet<String>,
    #[serde(default)]
    attributed_workspace_events: Vec<TaskAttributedWorkspaceEvent>,
    #[serde(default)]
    workspace_proof_scope_identity: String,
    #[serde(default)]
    workspace_event_history_complete: bool,
    #[serde(default)]
    source_capture_failed: bool,
    #[serde(default)]
    obligation: CompletionReviewObligationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskAttributedWorkspaceEvent {
    workspace_id: String,
    epoch: u64,
    actor_id: String,
    paths: Vec<String>,
    #[serde(default)]
    contracts: Vec<String>,
    #[serde(default)]
    actor_kind: Option<codex_agent_task_store::WorkspaceActorKind>,
    #[serde(default)]
    attribution_confidence: Option<codex_agent_task_store::AttributionConfidence>,
    #[serde(default)]
    relevance: WorkspaceEventRelevance,
    #[serde(default)]
    classified_scope_identity: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceEventRelevance {
    Relevant,
    Unrelated,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceProofScope {
    identity: String,
    paths: BTreeSet<String>,
    contracts: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserSourceRecord {
    pub(crate) source_id: String,
    pub(crate) message_id: String,
    pub(crate) source_kind: UserSourceKind,
    pub(crate) content_hash: String,
    pub(crate) source_ordinal: u64,
    pub(crate) content_ordinal: u64,
    pub(crate) exact_material: String,
    pub(crate) availability: UserSourceAvailability,
    pub(crate) completion_epoch: u64,
    #[serde(default)]
    pub(crate) introduced_manifest_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UserSourceKind {
    Text,
    Image,
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UserSourceAvailability {
    Available,
    Unavailable,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceMappingRevision {
    completion_epoch: u64,
    manifest_revision: u64,
    source_id: String,
    #[serde(default)]
    source_classification_contract_version: Option<String>,
    #[serde(default)]
    relationship_resolver_contract_version: Option<String>,
    mapping: SourceMapping,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SourceClassificationCacheKey {
    pub(crate) contract_version: String,
    pub(crate) source_kind: UserSourceKind,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceLocalClassificationKind {
    RequirementBearing,
    NonRequirement,
    RelationshipOnlyContext,
    UnavailableOrTruncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalSemanticCueKind {
    Assertion,
    ReplacementIntent,
    WithdrawalIntent,
    RelationshipOnlyContext,
    MandatoryCompletionReview,
    SupplementalCompletionReview,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalSemanticCue {
    pub(crate) kind: LocalSemanticCueKind,
    pub(crate) source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceLocalClassification {
    pub(crate) local_kind: SourceLocalClassificationKind,
    pub(crate) requirement_spans: Vec<SourceSpan>,
    pub(crate) local_semantic_cues: Vec<LocalSemanticCue>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceClassificationCacheEntry {
    pub(crate) contract_version: String,
    pub(crate) source_kind: UserSourceKind,
    pub(crate) content_hash: String,
    pub(crate) classification: SourceLocalClassification,
}

impl SourceClassificationCacheEntry {
    pub(crate) fn key(&self) -> SourceClassificationCacheKey {
        SourceClassificationCacheKey {
            contract_version: self.contract_version.clone(),
            source_kind: self.source_kind,
            content_hash: self.content_hash.clone(),
        }
    }
}

fn deserialize_source_classification_cache<'de, D>(
    deserializer: D,
) -> Result<Vec<SourceClassificationCacheEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    let mut by_key =
        BTreeMap::<SourceClassificationCacheKey, Option<SourceClassificationCacheEntry>>::new();
    for value in values {
        let Some(key) = source_classification_cache_key_from_value(&value) else {
            continue;
        };
        let entry = serde_json::from_value::<SourceClassificationCacheEntry>(value)
            .ok()
            .filter(source_classification_cache_entry_is_valid);
        match by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                slot.insert(None);
            }
        }
    }
    Ok(by_key.into_values().flatten().collect())
}

fn source_classification_cache_key_from_value(
    value: &Value,
) -> Option<SourceClassificationCacheKey> {
    let object = value.as_object()?;
    let key = SourceClassificationCacheKey {
        contract_version: object.get("contract_version")?.as_str()?.to_string(),
        source_kind: serde_json::from_value(object.get("source_kind")?.clone()).ok()?,
        content_hash: object.get("content_hash")?.as_str()?.to_string(),
    };
    source_classification_cache_key_is_valid(&key).then_some(key)
}

fn source_classification_cache_key_is_valid(key: &SourceClassificationCacheKey) -> bool {
    !key.contract_version.trim().is_empty()
        && key.content_hash.len() == 64
        && key
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn cache_span_is_structurally_valid(source_kind: UserSourceKind, span: &SourceSpan) -> bool {
    match (source_kind, span) {
        (UserSourceKind::Text, SourceSpan::Text { start, end }) => start < end,
        (UserSourceKind::Image, SourceSpan::Image { reference, .. }) => {
            !reference.trim().is_empty()
        }
        (UserSourceKind::Attachment, SourceSpan::Attachment { reference, .. }) => {
            !reference.trim().is_empty()
        }
        _ => false,
    }
}

fn source_classification_cache_entry_is_valid(entry: &SourceClassificationCacheEntry) -> bool {
    if !source_classification_cache_key_is_valid(&entry.key())
        || entry.classification.reason.trim().is_empty()
    {
        return false;
    }
    let spans = &entry.classification.requirement_spans;
    if spans
        .iter()
        .any(|span| !cache_span_is_structurally_valid(entry.source_kind, span))
        || spans.windows(2).any(|pair| pair[0] >= pair[1])
        || entry.classification.local_semantic_cues.iter().any(|cue| {
            cue.source_span
                .as_ref()
                .is_some_and(|span| !cache_span_is_structurally_valid(entry.source_kind, span))
        })
        || entry
            .classification
            .local_semantic_cues
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return false;
    }
    matches!(
        (entry.classification.local_kind, spans.is_empty()),
        (SourceLocalClassificationKind::RequirementBearing, false)
            | (SourceLocalClassificationKind::NonRequirement, true)
            | (SourceLocalClassificationKind::RelationshipOnlyContext, true)
            | (SourceLocalClassificationKind::UnavailableOrTruncated, true)
    )
}

fn canonical_source_classification_cache(
    entries: Vec<SourceClassificationCacheEntry>,
) -> Vec<SourceClassificationCacheEntry> {
    let mut by_key =
        BTreeMap::<SourceClassificationCacheKey, Option<SourceClassificationCacheEntry>>::new();
    for entry in entries {
        let key = entry.key();
        match by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(source_classification_cache_entry_is_valid(&entry).then_some(entry));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                slot.insert(None);
            }
        }
    }
    by_key.into_values().flatten().collect()
}

pub(crate) fn source_classification_cache_key(
    source: &UserSourceRecord,
) -> SourceClassificationCacheKey {
    SourceClassificationCacheKey {
        contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
        source_kind: source.source_kind,
        content_hash: source.content_hash.clone(),
    }
}

pub(crate) fn source_local_classification_is_valid_for_source(
    source: &UserSourceRecord,
    classification: &SourceLocalClassification,
) -> bool {
    let entry = SourceClassificationCacheEntry {
        contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
        source_kind: source.source_kind,
        content_hash: source.content_hash.clone(),
        classification: classification.clone(),
    };
    source_classification_cache_entry_is_valid(&entry)
        && classification
            .requirement_spans
            .iter()
            .chain(
                classification
                    .local_semantic_cues
                    .iter()
                    .filter_map(|cue| cue.source_span.as_ref()),
            )
            .all(|span| material_for_span(source, span).is_some())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceMapping {
    PendingClassification,
    RequirementBearing { requirement_ids: Vec<String> },
    NonRequirement { reason: String },
    SupersededContext { reason: String },
    UnavailableOrTruncated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RequirementManifestSnapshot {
    completion_epoch: u64,
    manifest_revision: u64,
    manifest_hash: String,
    requirements: Vec<RequirementRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequirementRecord {
    pub(crate) requirement_id: String,
    pub(crate) source_id: String,
    pub(crate) source_content_hash: String,
    pub(crate) source_span: SourceSpan,
    pub(crate) exact_material: String,
    pub(crate) status: RequirementStatus,
    pub(crate) superseded_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceSpan {
    Text {
        start: usize,
        end: usize,
    },
    Image {
        reference: String,
        region: Option<String>,
    },
    Attachment {
        reference: String,
        range: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequirementStatus {
    Active,
    Superseded,
    Withdrawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassifiedSourceKind {
    RequirementBearing,
    NonRequirement,
    SupersededContext,
    UnavailableOrTruncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedSource {
    pub(crate) source_id: String,
    pub(crate) kind: ClassifiedSourceKind,
    pub(crate) requirements: Vec<ClassifiedRequirement>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceMaterialization {
    /// Exactly one source-local projection for every unique current cache key.
    pub(crate) local_classifications:
        BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    /// Exactly one occurrence-specific relationship result for every current source.
    pub(crate) resolved_sources: Vec<ClassifiedSource>,
}

#[derive(Debug)]
struct PreparedSourceMaterialization {
    local_classifications: BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    requirements: Vec<RequirementRecord>,
    mappings: Vec<(String, SourceMapping)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedRequirement {
    pub(crate) source_span: SourceSpan,
    pub(crate) status: RequirementStatus,
    pub(crate) superseded_by: Option<ClassifiedRequirementRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ClassifiedRequirementRef {
    pub(crate) source_id: String,
    pub(crate) source_span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewCycle {
    cycle_id: String,
    manifest_revision: u64,
    parent_terminal_review_id: Option<String>,
    #[serde(default)]
    superseded_review_id: Option<String>,
    phase: CompletionReviewCyclePhase,
    correction_consumed: bool,
    #[serde(default)]
    manifest_gap_reconstructed: bool,
    accepted_review_id: Option<String>,
    accepted_dossier_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewCyclePhase {
    ClassificationPending,
    InitialReviewPending,
    CorrectionPending,
    RereviewPending,
    ProvisionalClean,
    TerminalPartial,
    TerminalBlocked,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewRisk {
    unresolved: bool,
    cycle_id: Option<String>,
    opened_at: Option<String>,
    resolved_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewAttemptKind {
    InitialReview,
    CorrectionEvidence,
    Rereview,
    TerminalClosure,
}

impl CompletionReviewAttemptKind {
    #[cfg(test)]
    #[allow(non_upper_case_globals)]
    pub(crate) const Initial: Self = Self::InitialReview;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReviewFindingReceipt {
    pub(crate) finding_id: String,
    pub(crate) requirement_ids: Vec<String>,
    pub(crate) lens: String,
    pub(crate) contract_surface: String,
    pub(crate) severity: String,
    pub(crate) evidence: String,
    pub(crate) smallest_correction: String,
    pub(crate) proof_route: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReviewDispositionReceipt {
    pub(crate) finding_id: String,
    pub(crate) disposition: String,
    pub(crate) evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RereviewFallbackReason {
    MissingBaseline,
    InvalidBaselineHash,
    InvalidRepairLineage,
    UnsupportedPathGrammar,
    InvalidPath,
    AmbiguousWindowsCase,
    SymlinkEscape,
    PathOutsideScope,
    SourceIdentityChanged,
    RequirementManifestChanged,
    PlanStructureChanged,
    ContractSurfaceOutsideScope,
    UnattributedMutation,
    UnrepresentableEvidenceChange,
    CommandLineageChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RereviewInputMode {
    Delta,
    FullFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StructuredContractSurface {
    pub(crate) kind: String,
    pub(crate) owner: String,
    pub(crate) identifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RepairPathScope {
    ExactFile {
        path: String,
    },
    DirectoryPrefix {
        path: String,
    },
    GeneratedPattern {
        grammar_version: u32,
        pattern: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairPathState {
    pub(crate) path: String,
    pub(crate) exists: bool,
    pub(crate) content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BaselineCommandBinding {
    pub(crate) sequence: u64,
    pub(crate) receipt_id: String,
    pub(crate) implementation_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairScope {
    pub(crate) path_grammar_version: u32,
    pub(crate) paths: Vec<RepairPathScope>,
    pub(crate) surfaces: Vec<StructuredContractSurface>,
    pub(crate) affected_requirement_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairBaseline {
    pub(crate) path_states: Vec<RepairPathState>,
    pub(crate) command_sequence_high_water_mark: u64,
    pub(crate) command_bindings: Vec<BaselineCommandBinding>,
    pub(crate) implementation_surfaces: Vec<StructuredContractSurface>,
    pub(crate) repair_scope: RepairScope,
    pub(crate) source_ledger_hash: String,
    pub(crate) requirement_manifest_hash: String,
    pub(crate) plan_structure_hash: String,
    pub(crate) default_child_mutation_identities: Vec<String>,
    pub(crate) typed_mutation_identities: Vec<String>,
    pub(crate) external_evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairPathChangeKind {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairPathChange {
    pub(crate) path: String,
    pub(crate) change: RepairPathChangeKind,
    pub(crate) before_exists: bool,
    pub(crate) before_hash: Option<String>,
    pub(crate) after_exists: bool,
    pub(crate) after_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairCommandDelta {
    pub(crate) sequence: u64,
    pub(crate) receipt_id: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) implementation_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InvalidatedCommandReceipt {
    pub(crate) sequence: u64,
    pub(crate) receipt_id: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairDelta {
    pub(crate) original_findings: Vec<CompletionReviewFindingReceipt>,
    pub(crate) required_disposition_finding_ids: Vec<String>,
    pub(crate) repair_instruction_hash: String,
    pub(crate) baseline_hash: String,
    pub(crate) candidate_implementation_identity: String,
    pub(crate) path_changes: Vec<RepairPathChange>,
    pub(crate) new_command_receipts: Vec<RepairCommandDelta>,
    pub(crate) invalidated_command_receipts: Vec<InvalidatedCommandReceipt>,
    pub(crate) affected_requirement_ids: Vec<String>,
    pub(crate) newly_realized_surfaces: Vec<StructuredContractSurface>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RereviewInput {
    pub(crate) input_mode: RereviewInputMode,
    pub(crate) baseline_hash: Option<String>,
    pub(crate) delta_hash: Option<String>,
    pub(crate) fallback_reasons: Vec<RereviewFallbackReason>,
    pub(crate) repair_instruction_hash: String,
    pub(crate) candidate_implementation_identity: String,
    pub(crate) delta: Option<RepairDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentRepairSnapshot {
    pub(crate) repository_root: String,
    pub(crate) path_states: Vec<RepairPathState>,
    pub(crate) command_receipts: Vec<RepairCommandDelta>,
    pub(crate) plan_structure_hash: String,
    pub(crate) declared_path_scopes: Vec<RepairPathScope>,
    pub(crate) implementation_surfaces: Vec<StructuredContractSurface>,
    pub(crate) default_child_mutation_identities: Vec<String>,
    pub(crate) typed_mutation_identities: Vec<String>,
    pub(crate) external_evidence_ids: Vec<String>,
    pub(crate) containment_errors: Vec<RereviewFallbackReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewReceiptV2 {
    review_id: String,
    attempt_kind: CompletionReviewAttemptKind,
    parent_review_id: Option<String>,
    superseded_review_id: Option<String>,
    candidate_mutation_revision: u64,
    candidate_hash: String,
    implementation_identity_hash: String,
    dossier_snapshot_id: String,
    user_source_ledger_hash: String,
    requirement_manifest_hash: String,
    #[serde(default)]
    attempt_identity: String,
    #[serde(default)]
    reviewer_contract_hash: String,
    findings: Vec<CompletionReviewFindingReceipt>,
    dispositions: Vec<CompletionReviewDispositionReceipt>,
    #[serde(default)]
    manifest_gaps: Vec<ManifestGapInput>,
    repair_instruction_hash: Option<String>,
    #[serde(default)]
    repair_baseline: Option<RepairBaseline>,
    #[serde(default)]
    baseline_hash: Option<String>,
    #[serde(default)]
    input_mode: Option<RereviewInputMode>,
    #[serde(default)]
    delta_hash: Option<String>,
    #[serde(default)]
    rereview_delta: Option<RepairDelta>,
    #[serde(default)]
    fallback_reasons: Vec<RereviewFallbackReason>,
    #[serde(default)]
    candidate_implementation_identity: Option<String>,
    #[serde(default)]
    rereview_audit_hash: Option<String>,
    #[serde(default)]
    requirement: CompletionReviewRequirement,
    #[serde(default)]
    disposition: CompletionReviewDisposition,
    #[serde(default)]
    attempted_outcome: Option<CompletionReviewAttemptedOutcome>,
    infrastructure_outcome: String,
    review_clean: bool,
    terminal_outcome: Option<String>,
    recorded_at: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewRequirement {
    Disabled,
    #[default]
    Supplemental,
    Mandatory,
}

impl CompletionReviewRequirement {
    fn from_obligation_mode(mode: &str) -> Self {
        match mode {
            "disabled" => Self::Disabled,
            "mandatory" => Self::Mandatory,
            _ => Self::Supplemental,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewDisposition {
    NotApplicable,
    PreflightSkipped,
    #[default]
    Attempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewAttemptedOutcome {
    Clean,
    ActionableFindings,
    InfrastructureFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewFindingInput {
    pub(crate) local_ordinal: u32,
    pub(crate) requirement_ids: Vec<String>,
    pub(crate) lens: String,
    pub(crate) contract_surface: String,
    pub(crate) severity: String,
    pub(crate) evidence: String,
    pub(crate) smallest_correction: String,
    pub(crate) proof_route: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewAttemptInput {
    pub(crate) attempt_kind: CompletionReviewAttemptKind,
    pub(crate) parent_review_id: Option<String>,
    pub(crate) superseded_review_id: Option<String>,
    pub(crate) findings: Vec<CompletionReviewFindingInput>,
    pub(crate) dispositions: Vec<CompletionReviewDispositionReceipt>,
    pub(crate) manifest_gaps: Vec<ManifestGapInput>,
    pub(crate) repair_instruction: Option<String>,
    pub(crate) repair_instruction_hash: Option<String>,
    pub(crate) infrastructure_outcome: String,
    pub(crate) review_clean: bool,
    pub(crate) terminal_outcome: Option<String>,
    pub(crate) attempt_identity: String,
    pub(crate) reviewer_contract_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewObligationState {
    mode: String,
    requirement_ids: Vec<String>,
    obligation_hash: String,
    required_attempt_identity: Option<String>,
    satisfied_attempt_identity: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompletionReviewObligationInput {
    pub(crate) mode: String,
    pub(crate) requirement_ids: Vec<String>,
    pub(crate) obligation_hash: String,
    pub(crate) required_attempt_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorCompletionReviewAttempt {
    Clean,
    Actionable,
    DeterministicInfrastructure,
}

fn completion_review_attempt_dimensions(
    attempt_kind: CompletionReviewAttemptKind,
    infrastructure_outcome: &str,
    review_clean: bool,
    has_actionable_result: bool,
) -> (
    CompletionReviewDisposition,
    Option<CompletionReviewAttemptedOutcome>,
) {
    if matches!(
        attempt_kind,
        CompletionReviewAttemptKind::CorrectionEvidence
            | CompletionReviewAttemptKind::TerminalClosure
    ) {
        return (CompletionReviewDisposition::NotApplicable, None);
    }
    if matches!(
        infrastructure_outcome,
        "capacity"
            | "spawn_model"
            | "oversized_request"
            | "persistence"
            | "input_unavailable_or_truncated"
            | "user_source_drift"
            | "repeated_or_invalid_manifest_gap"
            | "invalid_or_incomplete_dossier"
            | "unsupported_reviewer_configuration"
            | "self_review_prohibited"
            | "candidate_changed"
    ) {
        return (CompletionReviewDisposition::PreflightSkipped, None);
    }
    let attempted_outcome = if infrastructure_outcome != "ok" {
        CompletionReviewAttemptedOutcome::InfrastructureFailure
    } else if has_actionable_result {
        CompletionReviewAttemptedOutcome::ActionableFindings
    } else if review_clean {
        CompletionReviewAttemptedOutcome::Clean
    } else {
        CompletionReviewAttemptedOutcome::InfrastructureFailure
    };
    (
        CompletionReviewDisposition::Attempted,
        Some(attempted_outcome),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedReviewAttempt {
    pub(crate) review_id: String,
    pub(crate) findings: Vec<CompletionReviewFindingReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestGapInput {
    pub(crate) source_id: String,
    pub(crate) omitted_spans: Vec<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewDossier {
    pub(crate) document_revision: u64,
    pub(crate) root_task_id: String,
    pub(crate) completion_epoch: u64,
    pub(crate) manifest_revision: u64,
    pub(crate) sources: Vec<UserSourceRecord>,
    pub(crate) source_mappings: BTreeMap<String, SourceMapping>,
    pub(crate) source_classification_cache:
        BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    pub(crate) source_classification_current: bool,
    pub(crate) relationship_resolution_current: bool,
    pub(crate) mappings_classified: bool,
    pub(crate) source_capture_failed: bool,
    pub(crate) requirements: Vec<RequirementRecord>,
    pub(crate) user_source_ledger_hash: String,
    pub(crate) requirement_manifest_hash: String,
    pub(crate) implementation_identity_hash: String,
    pub(crate) dossier_snapshot_id: String,
    pub(crate) host_mutation_revision: u64,
    pub(crate) has_task_attributed_mutations: bool,
    pub(crate) evidence_gate: TaskCompletionGate,
    pub(crate) locally_obtainable_proof_routes: Vec<String>,
    pub(crate) reviewer_visible_evidence: Value,
    pub(crate) review_lens_selection_facts: ReviewLensSelectionFacts,
    pub(crate) authoritative_input_errors: Vec<String>,
    pub(crate) typed_quiescent: bool,
    pub(crate) default_children_quiescent: bool,
    pub(crate) candidate_completion: Option<String>,
    pub(crate) correction_consumed: bool,
    pub(crate) cycle_phase: Option<CompletionReviewCyclePhase>,
    pub(crate) active_cycle_id: Option<String>,
    pub(crate) cycle_parent_review_id: Option<String>,
    pub(crate) cycle_superseded_review_id: Option<String>,
    pub(crate) accepted_review_id: Option<String>,
    pub(crate) initial_review_id: Option<String>,
    pub(crate) initial_repair_instruction_hash: Option<String>,
    pub(crate) original_findings: Vec<CompletionReviewFindingReceipt>,
    pub(crate) manifest_gap_reconstructed: bool,
    pub(crate) current_repair_snapshot: CurrentRepairSnapshot,
    pub(crate) initial_repair_baseline: Option<RepairBaseline>,
    pub(crate) initial_repair_baseline_hash: Option<String>,
    pub(crate) rereview_input: Option<RereviewInput>,
}

/// Validated structured facts that the completion-review host may use to select
/// applicable review lenses. Free-form review material is intentionally absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ReviewLensSelectionFacts {
    pub(crate) risk_hints: Vec<String>,
    pub(crate) task_mutation_paths: Vec<String>,
    pub(crate) child_mutation_paths: Vec<String>,
    pub(crate) plan_edit_paths: Vec<String>,
    pub(crate) plan_runtime_paths: Vec<String>,
    pub(crate) surface_roles: Vec<String>,
    pub(crate) validation_asset_paths: Vec<String>,
    pub(crate) generated_artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AtomicReviewTransition<T> {
    Persisted(T),
    Superseded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskStartState {
    cwd: String,
    repository_root: String,
    commit_hash: Option<String>,
    branch: Option<String>,
    repository_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EvidencePlanStep {
    id: String,
    #[serde(default = "initial_plan_step_revision")]
    revision: u64,
    step: String,
    status: StepStatus,
    depends_on: Vec<String>,
    acceptance_criteria: Vec<String>,
    runtime_paths: Vec<String>,
    generated_artifacts: Vec<String>,
    risks: Vec<String>,
    requires_desktop_activation: bool,
    #[serde(default)]
    validation_route: Option<ValidationRoute>,
    #[serde(default)]
    external_validation_route: Option<ExternalValidationRouteInput>,
    #[serde(default)]
    validation_disposition: ValidationDisposition,
    #[serde(default)]
    source_owner: Option<String>,
    #[serde(default)]
    implementation_surfaces: Vec<String>,
    #[serde(default)]
    mutation_obligations: Vec<MutationObligationState>,
    #[serde(default)]
    validation_receipt_id: Option<String>,
    edit_paths: BTreeSet<String>,
}

const fn initial_plan_step_revision() -> u64 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PlanningEvidenceState {
    tier: PlanningTier,
    #[serde(default)]
    material_revision: u64,
    #[serde(default)]
    facts: BTreeMap<String, PlanningFactInput>,
    #[serde(default)]
    work_unit: Option<FocusedWorkUnit>,
    #[serde(default)]
    audit_history: Vec<PlanningAuditEntry>,
    #[serde(default)]
    outside_plan_actions: Vec<OutsidePlanAction>,
    #[serde(default)]
    counters: PlanningEvidenceCounters,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusedWorkUnit {
    id: String,
    source_owner: Option<String>,
    implementation_surfaces: Vec<String>,
    acceptance_criteria: Vec<String>,
    mutation_obligations: Vec<MutationObligationState>,
    validation_disposition: ValidationDisposition,
    validation_route: Option<ValidationRoute>,
    external_validation_route: Option<ExternalValidationRouteInput>,
    validation_receipt_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MutationObligationState {
    id: String,
    description: String,
    paths: Vec<String>,
    #[serde(default)]
    satisfied_paths: BTreeSet<String>,
    #[serde(default)]
    satisfied: bool,
}

impl From<MutationObligationInput> for MutationObligationState {
    fn from(input: MutationObligationInput) -> Self {
        Self {
            id: input.id,
            description: input.description,
            paths: input
                .paths
                .into_iter()
                .map(|path| normalize_slashes(&path))
                .collect(),
            satisfied_paths: BTreeSet::new(),
            satisfied: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PlanningAuditEntry {
    kind: String,
    id: String,
    reason: String,
    revision: u64,
    recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OutsidePlanAction {
    kind: String,
    action_id: String,
    recorded_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PlanningEvidenceCounters {
    initial_updates: u64,
    structural_revisions: u64,
    status_only_updates: u64,
    no_op_updates: u64,
    step_revisions: u64,
    step_removals: u64,
    fact_removals: u64,
    outside_plan_actions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActionAttributionKind {
    FocusedWorkUnit,
    PlannedStep,
    OutsidePlan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImplementationBatchAcknowledgement {
    step_id: String,
    implementation_revision: u64,
    acknowledged_at: String,
    /// Exact covered-file snapshot at the boundary. Empty coverage is
    /// repository-wide and relies on the authoritative repository revision and
    /// active-mutation checks at launch.
    covered_manifest: Vec<FileHashSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutoValidationCandidate {
    pub(crate) step_id: String,
    pub(crate) step_revision: u64,
    pub(crate) route: ValidationRoute,
    pub(crate) implementation_revision: u64,
    pub(crate) implementation_identity: String,
    pub(crate) leaf_implementation_identities: Vec<String>,
    pub(crate) repository_wide: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EditIntent {
    call_id: String,
    step_id: Option<String>,
    #[serde(default)]
    step_revision: Option<u64>,
    #[serde(default)]
    work_unit_id: Option<String>,
    #[serde(default)]
    attribution: Option<ActionAttributionKind>,
    started_at: String,
    completed_at: Option<String>,
    outcome: Option<String>,
    files: Vec<FileHashSnapshot>,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EditReceipt {
    id: String,
    call_id: String,
    step_id: Option<String>,
    #[serde(default)]
    step_revision: Option<u64>,
    #[serde(default)]
    work_unit_id: Option<String>,
    #[serde(default)]
    attribution: Option<ActionAttributionKind>,
    recorded_at: String,
    epoch: u64,
    outcome: String,
    files: Vec<FileHashTransition>,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileHashTransition {
    path: String,
    before_sha1: Option<String>,
    after_sha1: Option<String>,
    before_exists: bool,
    after_exists: bool,
    #[serde(default)]
    before_read_error: Option<String>,
    #[serde(default)]
    after_read_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileHashSnapshot {
    path: String,
    sha1: Option<String>,
    exists: bool,
    #[serde(default)]
    read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandReceipt {
    id: String,
    recorded_at: String,
    epoch: u64,
    step_id: Option<String>,
    #[serde(default)]
    step_revision: Option<u64>,
    #[serde(default)]
    work_unit_id: Option<String>,
    #[serde(default)]
    attribution: Option<ActionAttributionKind>,
    command: Vec<String>,
    cwd: String,
    exit_code: i32,
    timed_out: bool,
    duration_ms: u64,
    possible_mutation: bool,
    #[serde(default)]
    observed_mutation: bool,
    #[serde(default)]
    host_mutation_revision: Option<u64>,
    #[serde(default)]
    manifest_revision: Option<u64>,
    #[serde(default)]
    user_source_ledger_hash: Option<String>,
    #[serde(default)]
    requirement_manifest_hash: Option<String>,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
    #[serde(default)]
    validation_result: Option<ValidationResult>,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceCompleteness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalEvidenceReceipt {
    id: String,
    producer: String,
    producer_schema_version: u32,
    server_name: String,
    tool_name: String,
    call_id: String,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
    recorded_at: String,
    task_epoch: u64,
    step_id: Option<String>,
    #[serde(default)]
    step_revision: Option<u64>,
    #[serde(default)]
    work_unit_id: Option<String>,
    #[serde(default)]
    attribution: Option<ActionAttributionKind>,
    workspace_root_fingerprint: String,
    host_mutation_revision: Option<u64>,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
    provider_snapshot: Option<String>,
    tool_success: bool,
    payload_completeness: EvidenceCompleteness,
    truncated: bool,
    approximate: bool,
    limitations: Vec<String>,
    result_sha256: String,
    payload: Option<Value>,
    payload_artifact_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionReviewAuditReceipt {
    turn_id: String,
    recorded_at: String,
    evidence_epoch: u64,
    outcome: String,
    failure_category: Option<String>,
    finding_summary: Vec<String>,
    repair_injected: bool,
    #[serde(default)]
    measurements: CompletionReviewAuditMeasurements,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReviewAuditMeasurements {
    pub(crate) obligation_mode: String,
    pub(crate) obligation_hash: String,
    pub(crate) admission_result: String,
    pub(crate) preflight_result: String,
    pub(crate) attempt_identity: String,
    pub(crate) reviewer_contract_hash: String,
    pub(crate) failure_class: String,
    pub(crate) retry_disposition: String,
    pub(crate) elapsed_millis: u64,
    pub(crate) logical_generations: u64,
    pub(crate) physical_requests: u64,
    pub(crate) tool_calls: u64,
    pub(crate) findings: u64,
    pub(crate) actionable_findings: u64,
    pub(crate) repair_count: u64,
    pub(crate) rereview_count: u64,
    pub(crate) resulting_changes: bool,
    pub(crate) mandatory_proof_state: String,
    pub(crate) review_infrastructure_caused_partial: bool,
    pub(crate) validation_failures_prevented: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GeneratedArtifactRequirement {
    id: String,
    step_id: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvidenceRisk {
    id: String,
    description: String,
    source: String,
    blocking: bool,
    resolved: bool,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DesktopActivationReceipt {
    #[serde(default)]
    trusted_producer_version: u32,
    #[serde(default)]
    publisher_evidence_id: String,
    #[serde(default)]
    thread_id: String,
    epoch: u64,
    #[serde(default)]
    activation_obligation_identity: String,
    #[serde(default, alias = "recorded_at")]
    activation_timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binary_sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_evidence: Option<String>,
    #[serde(default)]
    expected_installed_executable_path: String,
    #[serde(default)]
    installed_executable_sha256: String,
    #[serde(default)]
    running_process_id: u32,
    #[serde(default)]
    running_process_identity: String,
    #[serde(default)]
    observed_running_executable_path: String,
    #[serde(default)]
    observed_running_executable_sha256: String,
    #[serde(default)]
    desktop_process_id: u32,
    #[serde(default)]
    desktop_process_identity: String,
    #[serde(default)]
    desktop_executable_path: String,
    #[serde(default)]
    initialization_observation_identity: String,
    #[serde(default)]
    post_restart_initialization_observation: String,
    #[serde(default)]
    observation_timestamp: String,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
    #[serde(default)]
    publish_identity: String,
    #[serde(default)]
    install_generation: u64,
    #[serde(default)]
    authoritative_install_evidence_hash: String,
    #[serde(default)]
    authenticated_host_channel_identity: String,
    #[serde(default)]
    challenge_identity: String,
    #[serde(default)]
    challenge_expires_at: String,
    #[serde(default)]
    publish_install_timestamp: String,
    #[serde(default)]
    bootstrap_consumed_timestamp: String,
    #[serde(default)]
    challenge_issued_timestamp: String,
    #[serde(default)]
    running_executable_observed_timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopPublishInstallEvidenceV1 {
    pub schema_version: u32,
    pub trusted_producer_version: u32,
    pub publisher_evidence_id: String,
    pub thread_id: String,
    pub evidence_epoch: u64,
    #[serde(rename = "implementationIdentity")]
    pub implementation_identity_hash: String,
    pub activation_obligation_identity: String,
    #[serde(rename = "publishId")]
    pub publish_identity: String,
    #[serde(default)]
    pub install_generation: u64,
    pub expected_installed_executable_path: String,
    #[serde(rename = "installedFileSha256")]
    pub installed_executable_sha256: String,
    #[serde(rename = "installationTimestamp")]
    pub issued_at: String,
    #[serde(default)]
    pub expires_at: String,
}

type AuthoritativeDesktopInstallEvidence = DesktopPublishInstallEvidenceV1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum DesktopInstallEvidenceAvailability {
    #[default]
    NoAuthenticatedHostTransport,
    AuthenticatedHostBootstrap,
}

/// Sealed source boundary for authoritative install evidence. Production may only create the
/// authenticated variant from the startup-consumed inherited bootstrap handle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum DesktopInstallEvidenceSource {
    #[default]
    NoAuthenticatedHostTransport,
    AuthenticatedHostBootstrap {
        channel_identity: String,
        peer_identity: String,
        evidence: Box<AuthoritativeDesktopInstallEvidence>,
    },
}

impl DesktopInstallEvidenceSource {
    fn availability(&self) -> DesktopInstallEvidenceAvailability {
        match self {
            Self::NoAuthenticatedHostTransport => {
                DesktopInstallEvidenceAvailability::NoAuthenticatedHostTransport
            }
            Self::AuthenticatedHostBootstrap { .. } => {
                DesktopInstallEvidenceAvailability::AuthenticatedHostBootstrap
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopRunningProcessObservation {
    process_id: u32,
    process_identity: String,
    executable_path: String,
    executable_sha256: String,
    observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDesktopActivationChallenge {
    challenge_identity: String,
    nonce: String,
    thread_id: String,
    evidence_epoch: u64,
    implementation_identity_hash: String,
    activation_obligation_identity: String,
    publisher_evidence_id: String,
    authoritative_install_evidence_hash: String,
    publish_identity: String,
    install_generation: u64,
    expected_installed_executable_path: String,
    installed_executable_sha256: String,
    running_process: DesktopRunningProcessObservation,
    authenticated_host_channel_identity: String,
    authenticated_host_peer_identity: String,
    issued_at: String,
    expires_at: String,
    bootstrap_consumed_at: String,
    monotonic_deadline: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopActivationAcknowledgement {
    challenge_identity: String,
    authenticated_host_channel_identity: String,
    initialized_process_id: u32,
    initialized_process_identity: String,
    desktop_process_id: u32,
    desktop_process_identity: String,
    desktop_executable_path: String,
    initialization_observation_identity: String,
    observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveDesktopActivationProof {
    receipt_hash: String,
    evidence_epoch: u64,
    implementation_identity_hash: String,
    activation_obligation_identity: String,
    authoritative_install_evidence_hash: String,
    publish_identity: String,
    install_generation: u64,
    authenticated_host_channel_identity: String,
    running_process_id: u32,
    running_process_identity: String,
    fresh_until: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DesktopActivationRuntimeState {
    install_evidence_source: DesktopInstallEvidenceSource,
    pending_challenge: Option<PendingDesktopActivationChallenge>,
    live_proof: Option<LiveDesktopActivationProof>,
    recorded_challenges: BTreeMap<String, RecordedDesktopActivationChallenge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedDesktopActivationChallenge {
    request_hash: String,
    receipt: DesktopActivationReceipt,
    recorded_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DesktopActivationRuntimeSnapshot {
    availability: DesktopInstallEvidenceAvailability,
    current_install_evidence_hash: Option<String>,
    live_proof: Option<LiveDesktopActivationProof>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopActivationVerificationError {
    NoAuthenticatedHostTransport,
    InvalidAuthoritativeEvidence,
    AuthoritativeEvidenceStale,
    ImplementationIdentityMismatch,
    RunningExecutableMismatch,
    RunningProcessIdentityMissing,
    ChallengeMissingOrConsumed,
    ChallengeExpired,
    ChallengeIdentityMismatch,
    AuthenticatedChannelMismatch,
    InitializedProcessMismatch,
    InvalidDesktopObservation,
    ActivationObligationChanged,
    ChallengeAlreadyRecordedWithDifferentPayload,
    PersistenceFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopActivationObligation {
    pub thread_id: String,
    pub evidence_epoch: u64,
    pub implementation_identity: String,
    pub activation_obligation_identity: String,
    pub requiring_plan_step_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActivationChallenge {
    pub challenge_id: String,
    pub thread_id: String,
    pub evidence_epoch: u64,
    pub implementation_identity: String,
    pub activation_obligation_identity: String,
    pub publisher_evidence_id: String,
    pub expected_installed_executable_path: String,
    pub expected_installed_executable_sha256: String,
    pub publish_id: String,
    pub issued_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopActivationRecordObservation {
    pub challenge_id: String,
    pub desktop_process_id: u32,
    pub desktop_executable_path: String,
    pub observation_timestamp: String,
    pub initialization_observation_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopActivationRecordResult {
    pub challenge_id: String,
    pub recorded_at: String,
    pub already_recorded: bool,
}

impl DesktopActivationRuntimeState {
    fn snapshot(&self) -> DesktopActivationRuntimeSnapshot {
        let current_install_evidence_hash = match &self.install_evidence_source {
            DesktopInstallEvidenceSource::NoAuthenticatedHostTransport => None,
            DesktopInstallEvidenceSource::AuthenticatedHostBootstrap {
                channel_identity,
                peer_identity,
                evidence,
            } => Some(desktop_install_evidence_hash(
                evidence,
                channel_identity,
                peer_identity,
            )),
        };
        DesktopActivationRuntimeSnapshot {
            availability: self.install_evidence_source.availability(),
            current_install_evidence_hash,
            live_proof: self.live_proof.clone(),
        }
    }

    fn prepare_challenge(
        &mut self,
        obligation: &DesktopActivationObligation,
        running_process: DesktopRunningProcessObservation,
        nonce: &str,
        bootstrap_consumed_at: &str,
        now: DateTime<Utc>,
    ) -> Result<PendingDesktopActivationChallenge, DesktopActivationVerificationError> {
        let DesktopInstallEvidenceSource::AuthenticatedHostBootstrap {
            channel_identity,
            peer_identity,
            evidence,
        } = &self.install_evidence_source
        else {
            return Err(DesktopActivationVerificationError::NoAuthenticatedHostTransport);
        };
        validate_authoritative_desktop_install_evidence(
            evidence,
            channel_identity,
            peer_identity,
            obligation,
            now,
        )?;
        let bootstrap_consumed = parse_desktop_timestamp(bootstrap_consumed_at)
            .filter(|consumed| *consumed <= now)
            .ok_or(DesktopActivationVerificationError::InvalidAuthoritativeEvidence)?;
        let installed_at = parse_desktop_timestamp(&evidence.issued_at)
            .ok_or(DesktopActivationVerificationError::InvalidAuthoritativeEvidence)?;
        if installed_at > bootstrap_consumed {
            return Err(DesktopActivationVerificationError::AuthoritativeEvidenceStale);
        }
        if let Some(existing) = self.pending_challenge.as_ref()
            && existing.thread_id == obligation.thread_id
            && existing.evidence_epoch == obligation.evidence_epoch
            && existing.implementation_identity_hash == obligation.implementation_identity
            && existing.activation_obligation_identity == obligation.activation_obligation_identity
            && existing.publisher_evidence_id == evidence.publisher_evidence_id
            && Instant::now() <= existing.monotonic_deadline
        {
            return Ok(existing.clone());
        }
        self.pending_challenge = None;
        self.live_proof = None;
        let running_observed_at = parse_desktop_timestamp(&running_process.observed_at);
        if running_process.process_id == 0
            || running_process.process_identity.trim().is_empty()
            || running_observed_at
                .is_none_or(|observed_at| observed_at < bootstrap_consumed || observed_at > now)
        {
            return Err(DesktopActivationVerificationError::RunningProcessIdentityMissing);
        }
        if !desktop_paths_match(
            &evidence.expected_installed_executable_path,
            &running_process.executable_path,
        ) || !evidence
            .installed_executable_sha256
            .eq_ignore_ascii_case(&running_process.executable_sha256)
        {
            return Err(DesktopActivationVerificationError::RunningExecutableMismatch);
        }
        if nonce.trim().is_empty() {
            return Err(DesktopActivationVerificationError::InvalidAuthoritativeEvidence);
        }
        let authoritative_install_evidence_hash =
            desktop_install_evidence_hash(evidence, channel_identity, peer_identity);
        let expires_at = now + ChronoDuration::seconds(DESKTOP_ACTIVATION_CHALLENGE_TTL_SECONDS);
        let issued_at = now.to_rfc3339();
        let expires_at = expires_at.to_rfc3339();
        let challenge_identity = canonical_hash(
            DESKTOP_ACTIVATION_CHALLENGE_CANONICAL_FORMAT,
            &serde_json::json!({
                "nonce": nonce,
                "threadId": obligation.thread_id,
                "evidenceEpoch": obligation.evidence_epoch,
                "implementationIdentity": obligation.implementation_identity,
                "activationObligationIdentity": obligation.activation_obligation_identity,
                "authoritativeInstallEvidence": authoritative_install_evidence_hash,
                "publisherEvidenceId": evidence.publisher_evidence_id,
                "publishIdentity": evidence.publish_identity,
                "runningProcessId": running_process.process_id,
                "runningProcessIdentity": running_process.process_identity,
                "authenticatedHostChannel": channel_identity,
                "authenticatedHostPeer": peer_identity,
                "issuedAt": issued_at,
                "expiresAt": expires_at,
            }),
        );
        let challenge = PendingDesktopActivationChallenge {
            challenge_identity,
            nonce: nonce.to_string(),
            thread_id: obligation.thread_id.clone(),
            evidence_epoch: obligation.evidence_epoch,
            implementation_identity_hash: obligation.implementation_identity.clone(),
            activation_obligation_identity: obligation.activation_obligation_identity.clone(),
            publisher_evidence_id: evidence.publisher_evidence_id.clone(),
            authoritative_install_evidence_hash,
            publish_identity: evidence.publish_identity.clone(),
            install_generation: evidence.install_generation,
            expected_installed_executable_path: evidence.expected_installed_executable_path.clone(),
            installed_executable_sha256: evidence.installed_executable_sha256.clone(),
            running_process,
            authenticated_host_channel_identity: channel_identity.clone(),
            authenticated_host_peer_identity: peer_identity.clone(),
            issued_at,
            expires_at,
            bootstrap_consumed_at: bootstrap_consumed_at.to_string(),
            monotonic_deadline: Instant::now()
                + std::time::Duration::from_secs(DESKTOP_ACTIVATION_CHALLENGE_TTL_SECONDS as u64),
        };
        self.pending_challenge = Some(challenge.clone());
        Ok(challenge)
    }

    fn complete_challenge(
        &mut self,
        acknowledgement: DesktopActivationAcknowledgement,
        now: DateTime<Utc>,
    ) -> Result<DesktopActivationReceipt, DesktopActivationVerificationError> {
        let challenge = self
            .pending_challenge
            .take()
            .ok_or(DesktopActivationVerificationError::ChallengeMissingOrConsumed)?;
        if Instant::now() > challenge.monotonic_deadline {
            return Err(DesktopActivationVerificationError::ChallengeExpired);
        }
        if acknowledgement.challenge_identity != challenge.challenge_identity {
            return Err(DesktopActivationVerificationError::ChallengeIdentityMismatch);
        }
        if acknowledgement.authenticated_host_channel_identity
            != challenge.authenticated_host_channel_identity
        {
            return Err(DesktopActivationVerificationError::AuthenticatedChannelMismatch);
        }
        if acknowledgement.initialized_process_id != challenge.running_process.process_id
            || acknowledgement.initialized_process_identity
                != challenge.running_process.process_identity
        {
            return Err(DesktopActivationVerificationError::InitializedProcessMismatch);
        }
        let observation_timestamp = parse_desktop_timestamp(&acknowledgement.observed_at)
            .filter(|observed_at| {
                parse_desktop_timestamp(&challenge.issued_at)
                    .is_some_and(|issued_at| *observed_at >= issued_at && *observed_at <= now)
            })
            .ok_or(DesktopActivationVerificationError::InvalidDesktopObservation)?;
        if acknowledgement.desktop_process_id == 0
            || acknowledgement.desktop_process_identity.trim().is_empty()
            || !Path::new(&acknowledgement.desktop_executable_path).is_absolute()
            || acknowledgement
                .initialization_observation_identity
                .trim()
                .is_empty()
        {
            return Err(DesktopActivationVerificationError::InvalidDesktopObservation);
        }
        let receipt = DesktopActivationReceipt {
            trusted_producer_version: DESKTOP_INSTALL_EVIDENCE_SCHEMA_VERSION,
            publisher_evidence_id: challenge.publisher_evidence_id.clone(),
            thread_id: challenge.thread_id.clone(),
            epoch: challenge.evidence_epoch,
            activation_obligation_identity: challenge.activation_obligation_identity.clone(),
            activation_timestamp: now.to_rfc3339(),
            process_path: None,
            binary_sha1: None,
            runtime_evidence: Some("authenticated_host_bootstrap_v1".to_string()),
            expected_installed_executable_path: challenge
                .expected_installed_executable_path
                .clone(),
            installed_executable_sha256: challenge.installed_executable_sha256.clone(),
            running_process_id: challenge.running_process.process_id,
            running_process_identity: challenge.running_process.process_identity.clone(),
            observed_running_executable_path: challenge.running_process.executable_path.clone(),
            observed_running_executable_sha256: challenge.running_process.executable_sha256.clone(),
            desktop_process_id: acknowledgement.desktop_process_id,
            desktop_process_identity: acknowledgement.desktop_process_identity,
            desktop_executable_path: acknowledgement.desktop_executable_path,
            initialization_observation_identity: acknowledgement
                .initialization_observation_identity,
            post_restart_initialization_observation:
                "authenticated host initialized the exact app-server process".to_string(),
            observation_timestamp: observation_timestamp.to_rfc3339(),
            implementation_identity_hash: Some(challenge.implementation_identity_hash.clone()),
            publish_identity: challenge.publish_identity.clone(),
            install_generation: challenge.install_generation,
            authoritative_install_evidence_hash: challenge
                .authoritative_install_evidence_hash
                .clone(),
            authenticated_host_channel_identity: challenge
                .authenticated_host_channel_identity
                .clone(),
            challenge_identity: challenge.challenge_identity,
            challenge_expires_at: challenge.expires_at.clone(),
            publish_install_timestamp: match &self.install_evidence_source {
                DesktopInstallEvidenceSource::AuthenticatedHostBootstrap { evidence, .. } => {
                    evidence.issued_at.clone()
                }
                DesktopInstallEvidenceSource::NoAuthenticatedHostTransport => String::new(),
            },
            bootstrap_consumed_timestamp: challenge.bootstrap_consumed_at,
            challenge_issued_timestamp: challenge.issued_at.clone(),
            running_executable_observed_timestamp: challenge.running_process.observed_at.clone(),
        };
        self.live_proof = Some(LiveDesktopActivationProof {
            receipt_hash: desktop_activation_receipt_hash(&receipt),
            evidence_epoch: receipt.epoch,
            implementation_identity_hash: challenge.implementation_identity_hash,
            activation_obligation_identity: challenge.activation_obligation_identity,
            authoritative_install_evidence_hash: challenge.authoritative_install_evidence_hash,
            publish_identity: challenge.publish_identity,
            install_generation: challenge.install_generation,
            authenticated_host_channel_identity: challenge.authenticated_host_channel_identity,
            running_process_id: challenge.running_process.process_id,
            running_process_identity: challenge.running_process.process_identity,
            fresh_until: challenge.expires_at,
        });
        Ok(receipt)
    }
}

fn new_completion_review_ledger(root_task_id: &str) -> CompletionReviewLedgerV2 {
    CompletionReviewLedgerV2 {
        root_task_id: root_task_id.to_string(),
        completion_epoch: 1,
        manifest_revision: 0,
        next_source_ordinal: 1,
        source_records: BTreeMap::new(),
        mapping_revisions: Vec::new(),
        manifest_snapshots: Vec::new(),
        active_review_cycle: None,
        review_risk: CompletionReviewRisk {
            unresolved: false,
            cycle_id: None,
            opened_at: None,
            resolved_at: None,
        },
        receipts: Vec::new(),
        next_review_sequence: 1,
        last_terminal_closure: None,
        last_workspace_event_epoch: 0,
        workspace_event_baseline_epoch: 0,
        typed_assignment_baseline: BTreeSet::new(),
        attributed_workspace_events: Vec::new(),
        workspace_proof_scope_identity: String::new(),
        workspace_event_history_complete: false,
        source_capture_failed: false,
        obligation: CompletionReviewObligationState::default(),
    }
}

fn render_compaction_task_state(document: &TaskEvidenceDocument) -> String {
    let unresolved_steps = document
        .plan
        .iter()
        .filter(|step| {
            !matches!(
                step.status,
                StepStatus::Passed | StepStatus::Completed | StepStatus::Skipped
            )
        })
        .map(|step| {
            let active = if document.active_step_id.as_deref() == Some(step.id.as_str()) {
                " active"
            } else {
                ""
            };
            format!(
                "- {} [{}{}]: {}",
                step.id,
                step_status_name(&step.status),
                active,
                step.step
            )
        })
        .collect::<Vec<_>>();

    let mut goal = document
        .plan
        .iter()
        .filter(|step| {
            !matches!(
                step.status,
                StepStatus::Passed | StepStatus::Completed | StepStatus::Skipped
            )
        })
        .flat_map(|step| {
            step.acceptance_criteria
                .iter()
                .map(move |criterion| format!("- {}: {criterion}", step.id))
        })
        .collect::<Vec<_>>();
    if goal.is_empty() {
        goal.push("- Complete the active durable task plan and user request.".to_string());
    }

    let mut current_state = unresolved_steps.clone();
    current_state.extend(
        document
            .latest_file_hashes
            .keys()
            .map(|path| format!("- changed path: {path}")),
    );

    let completed_work = document
        .plan
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                StepStatus::Passed | StepStatus::Completed | StepStatus::Skipped
            )
        })
        .map(|step| {
            format!(
                "- {} [{}]: {} (validation receipt: {})",
                step.id,
                step_status_name(&step.status),
                step.step,
                step.validation_receipt_id.as_deref().unwrap_or("none")
            )
        })
        .collect::<Vec<_>>();

    let mut unresolved_work = unresolved_steps;
    for step in document.plan.iter().filter(|step| {
        !matches!(
            step.status,
            StepStatus::Passed | StepStatus::Completed | StepStatus::Skipped
        )
    }) {
        unresolved_work.extend(
            step.mutation_obligations
                .iter()
                .filter(|obligation| !obligation.satisfied)
                .map(|obligation| {
                    format!(
                        "- {} obligation {}: {} (paths: {})",
                        step.id,
                        obligation.id,
                        obligation.description,
                        obligation.paths.join(", ")
                    )
                }),
        );
    }
    let stale_facts = document
        .planning
        .facts
        .values()
        .filter(|fact| !fact.dependencies_current)
        .collect::<Vec<_>>();
    if !stale_facts.is_empty() {
        unresolved_work.push("- Invalidated evidence claims (not authoritative):".to_string());
        unresolved_work.extend(stale_facts.into_iter().map(|fact| {
            format!(
                "  - {}: stale after a dependency changed; re-establish `{}`",
                fact.id, fact.value
            )
        }));
    }
    unresolved_work.extend(
        document
            .risks
            .iter()
            .filter(|risk| !risk.resolved && risk.blocking)
            .map(|risk| format!("- blocking risk {}: {}", risk.id, risk.description)),
    );
    let warnings = document
        .risks
        .iter()
        .filter(|risk| !risk.resolved && !risk.blocking)
        .map(|risk| format!("- risk {}: {}", risk.id, risk.description))
        .collect::<Vec<_>>();
    let locked_decisions = document
        .locked_user_decisions
        .iter()
        .filter(|decision| {
            !document.locked_user_decisions.iter().any(|candidate| {
                candidate.supersedes.as_deref() == Some(decision.decision_id.as_str())
            })
        })
        .map(|decision| {
            format!(
                "- {}: {} (question: {}; turn: {})",
                decision.header,
                decision.answers.join(", "),
                decision.question,
                decision.turn_id
            )
        })
        .collect::<Vec<_>>();

    let mut evidence = vec![
        "- Recorded facts remain evidence claims; do not treat durable storage as proof."
            .to_string(),
        "- Recorded evidence claims:".to_string(),
    ];
    evidence.extend(
        document
            .planning
            .facts
            .values()
            .filter(|fact| fact.dependencies_current)
            .map(|fact| {
                let source = fact.source.as_deref().unwrap_or("not recorded");
                let dependencies = if fact.depends_on_paths.is_empty() {
                    "repository-wide (legacy/unspecified)".to_string()
                } else {
                    fact.depends_on_paths.join(", ")
                };
                format!(
                    "- {}: {} (provenance: {}; source: {source}; depends on: {dependencies})",
                    fact.id,
                    fact.value,
                    fact.provenance.as_str()
                )
            }),
    );
    evidence.extend(
        document
            .command_receipts
            .iter()
            .rev()
            .take(6)
            .map(|receipt| {
                format!(
                    "- command {}: exit={} timed_out={} freshness={} step={} recorded_at={}",
                    receipt.id,
                    receipt.exit_code,
                    receipt.timed_out,
                    compaction_command_freshness(document, receipt),
                    receipt.step_id.as_deref().unwrap_or("unattributed"),
                    receipt.recorded_at
                )
            }),
    );

    let next_action = document
        .active_step_id
        .as_deref()
        .and_then(|id| document.plan.iter().find(|step| step.id == id))
        .or_else(|| {
            document.plan.iter().find(|step| {
                !matches!(
                    step.status,
                    StepStatus::Passed | StepStatus::Completed | StepStatus::Skipped
                )
            })
        })
        .map(|step| vec![format!("- {}: {}", step.id, step.step)])
        .unwrap_or_else(|| vec!["- No unresolved durable plan action.".to_string()]);

    let sections = [
        bounded_compaction_section("## Goal", goal, 250),
        bounded_compaction_section("## Current state", current_state, 400),
        bounded_compaction_section("## Completed work", completed_work, 300),
        bounded_compaction_section("## Locked decisions", locked_decisions, 300),
        bounded_compaction_section("## Unresolved work", unresolved_work, 400),
        bounded_compaction_section("## Warnings", warnings, 200),
        bounded_compaction_section("## Evidence", evidence, 350),
        bounded_compaction_section("## Next action", next_action, 150),
    ];

    codex_utils_output_truncation::truncate_text_to_token_ceiling(
        &sections.join("\n\n"),
        COMPACTION_TASK_STATE_MAX_TOKENS,
    )
}

fn empty_compaction_task_state() -> String {
    [
        "## Goal\n- Continue the current user request.",
        "## Current state\n- Resume from the retained compacted history.",
        "## Completed work\n- None recorded in durable task evidence.",
        "## Unresolved work\n- Reconcile the retained user tail with the opaque remote checkpoint.",
        "## Evidence\n- Remote compaction completed and retained its checkpoint.",
        "## Next action\n- Continue the current request from retained history.",
    ]
    .join("\n\n")
}

fn task_is_tracked_for_compaction(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        || !document.locked_user_decisions.is_empty()
        || !document.edit_receipts.is_empty()
        || document
            .command_receipts
            .iter()
            .any(|receipt| receipt.observed_mutation)
        || document.risks.iter().any(|risk| {
            matches!(
                risk.source.as_str(),
                "task_evidence_storage" | "completion_review_recovery"
            ) && !risk.resolved
        })
}

fn task_is_tracked(document: &TaskEvidenceDocument) -> bool {
    task_is_tracked_for_compaction(document)
}

fn compaction_command_freshness(
    document: &TaskEvidenceDocument,
    receipt: &CommandReceipt,
) -> &'static str {
    if command_receipt_has_current_proof_identity(document, receipt) {
        "current"
    } else if receipt.epoch != document.evidence_epoch
        || receipt.host_mutation_revision != Some(document.host_mutation_revision)
    {
        "stale"
    } else {
        "unknown"
    }
}

fn bounded_compaction_section(title: &str, lines: Vec<String>, max_tokens: usize) -> String {
    if lines.is_empty() {
        return format!("{title}\n- none recorded");
    }
    format!(
        "{title}\n{}",
        codex_utils_output_truncation::truncate_text_to_token_ceiling(
            &lines.join("\n"),
            max_tokens
        )
    )
}

fn step_status_name(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "pending",
        StepStatus::InProgress => "in_progress",
        StepStatus::Implemented => "implemented",
        StepStatus::Passed | StepStatus::Completed => "passed",
        StepStatus::Blocked => "blocked",
        StepStatus::Skipped => "skipped",
    }
}

impl TaskEvidenceLedger {
    pub(crate) async fn load_or_new(codex_home: PathBuf, thread_id: ThreadId, cwd: &Path) -> Self {
        let (mode, repo_root) = if let Some(repo_root) = find_kd4_repo_root(cwd) {
            (TaskEvidenceMode::Kd4Completion, repo_root)
        } else if let Some(repo_root) = get_git_repo_root(cwd) {
            (TaskEvidenceMode::EvidenceOnly, repo_root)
        } else {
            return Self::disabled();
        };
        let repo_root = canonical_repository_root(&repo_root);
        let evidence_path = codex_home
            .join("task-evidence")
            .join(format!("{thread_id}.json"));
        let now = timestamp();
        let thread_id_text = thread_id.to_string();
        let repository_root = repo_root.to_string_lossy().into_owned();

        let existing = load_existing_document(&evidence_path, &thread_id_text, &repo_root).await;
        let mut storage_failure_reason = None;
        let mut recovery_failure_reason = None;
        let existing = match existing {
            ExistingDocument::Loaded {
                document,
                legacy_completion_model,
            } => Some((*document, legacy_completion_model)),
            ExistingDocument::Missing => None,
            ExistingDocument::NewerSchema { schema_version } => {
                warn!(
                    "disabling task evidence because {} uses newer schema version {schema_version}; refusing to modify it",
                    evidence_path.display()
                );
                return Self::disabled();
            }
            ExistingDocument::Rejected { kind, reason } => {
                recovery_failure_reason = Some(format!(
                    "rejected task evidence was removed from the active lineage: {reason}"
                ));
                let quarantine = quarantine_evidence_file(&evidence_path, kind).await;
                match quarantine {
                    Ok(path) => warn!(
                        "preserved rejected KD4 task evidence at {}: {reason}",
                        path.display()
                    ),
                    Err(err) => {
                        let failure = format!(
                            "rejected task evidence could not be quarantined ({reason}; quarantine failed: {err})"
                        );
                        warn!(
                            "refusing to overwrite rejected KD4 task evidence at {}: {failure}",
                            evidence_path.display()
                        );
                        storage_failure_reason = Some(failure);
                    }
                }
                None
            }
        };
        let document = if let Some((mut document, legacy_completion_model)) = existing {
            migrate_document_with_completion_model(&mut document, legacy_completion_model);
            for receipt in &mut document.terminalization_receipts {
                if receipt.delivery_state.is_authoritative_claim()
                    && (!receipt.app_server_acknowledged
                        || !receipt.runtime_status_converged
                        || !receipt.rollout_mirrored
                        || !receipt.parent_notification_completed
                        || !receipt.post_terminal_cleanup_completed
                        || !receipt.active_turn_detached
                        || !receipt.terminal_interaction_released)
                {
                    // A durable terminal decision is immutable, but delivery and post-terminal
                    // effects remain independently recoverable. Do not manufacture release or
                    // delivery success merely because the ledger was reloaded.
                    receipt.recovery_state = TerminalRecoveryState::Pending;
                    receipt.updated_at = now.clone();
                }
            }
            document.start.repository_root.clone_from(&repository_root);
            document.updated_at = now;
            document.revision = document.revision.saturating_add(1);
            document
        } else {
            let git = collect_git_info(&repo_root).await;
            let mut risks = Vec::new();
            if let Some(reason) = storage_failure_reason.as_deref() {
                risks.push(task_evidence_storage_risk(reason, 0));
            }
            if let Some(reason) = recovery_failure_reason.as_deref() {
                risks.push(task_evidence_recovery_risk(reason, 0));
            }
            TaskEvidenceDocument {
                schema_version: TASK_EVIDENCE_SCHEMA_VERSION,
                revision: 1,
                thread_id: thread_id_text.clone(),
                started_at: now.clone(),
                updated_at: now,
                start: TaskStartState {
                    cwd: cwd.to_string_lossy().into_owned(),
                    repository_root,
                    commit_hash: git
                        .as_ref()
                        .and_then(|info| info.commit_hash.as_ref())
                        .map(|sha| sha.0.clone()),
                    branch: git.as_ref().and_then(|info| info.branch.clone()),
                    repository_url: git.and_then(|info| info.repository_url),
                },
                evidence_epoch: 0,
                last_mutation_at: None,
                planning: PlanningEvidenceState::default(),
                plan: Vec::new(),
                active_step_id: None,
                batch_acknowledgement: None,
                edit_intents: Vec::new(),
                edit_receipts: Vec::new(),
                command_receipts: Vec::new(),
                external_evidence: Vec::new(),
                completion_review_receipts: Vec::new(),
                generated_artifact_requirements: Vec::new(),
                latest_generated_artifact_hashes: BTreeMap::new(),
                latest_file_hashes: BTreeMap::new(),
                risks,
                desktop_activation_receipt: None,
                next_edit_receipt_sequence: initial_receipt_sequence(),
                next_command_receipt_sequence: initial_receipt_sequence(),
                next_external_evidence_receipt_sequence: initial_receipt_sequence(),
                host_mutation_revision: 0,
                completion_review_v2: Some(new_completion_review_ledger(&thread_id_text)),
                source_classification_cache: Vec::new(),
                terminalization_receipts: Vec::new(),
                locked_user_decisions: Vec::new(),
                final_proof: FinalProofStateV1::default(),
                completion: None,
            }
        };
        if mode == TaskEvidenceMode::EvidenceOnly && storage_failure_reason.is_some() {
            warn!(
                "disabling evidence-only task ledger because rejected evidence could not be safely replaced"
            );
            return Self::disabled();
        }
        let writable_evidence_path = storage_failure_reason.is_none().then_some(evidence_path);
        let source_capture_failed = document
            .completion_review_v2
            .as_ref()
            .is_some_and(|ledger| ledger.source_capture_failed);
        let ledger = Self {
            mode,
            codex_home: Some(codex_home.clone()),
            thread_id: Some(thread_id_text.clone()),
            evidence_path: writable_evidence_path,
            repo_root: Some(repo_root),
            document: Arc::new(Mutex::new(Some(document.clone()))),
            persistence_gate: Arc::new(Semaphore::new(1)),
            external_evidence_gate: Arc::new(Semaphore::new(1)),
            freshness_gate: Arc::new(Semaphore::new(1)),
            freshness_state: Arc::new(std::sync::Mutex::new(FreshnessState::default())),
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            source_capture_failed: Arc::new(AtomicBool::new(source_capture_failed)),
            desktop_activation_gate: Arc::new(Semaphore::new(1)),
            desktop_activation_runtime: Arc::new(std::sync::Mutex::new(
                DesktopActivationRuntimeState::default(),
            )),
            #[cfg(test)]
            persistence_test_control: Arc::new(std::sync::Mutex::new(None)),
        };
        if storage_failure_reason.is_none() {
            let persisted = ledger.persist_document(&document).await;
            if mode == TaskEvidenceMode::EvidenceOnly && persisted != PersistOutcome::Persisted {
                warn!(
                    "disabling evidence-only task ledger because initial persistence could not be established"
                );
                return Self::disabled();
            }
            if mode == TaskEvidenceMode::Kd4Completion && persisted == PersistOutcome::Failed {
                let mut guard = ledger.document.lock().await;
                if let Some(document) = guard.as_mut() {
                    upsert_risk(
                        document,
                        task_evidence_storage_risk(
                            "initial task-evidence document could not be durably persisted",
                            document.evidence_epoch,
                        ),
                    );
                    document.updated_at = timestamp();
                    document.revision = document.revision.saturating_add(1);
                }
            }
        }
        let referenced_artifact_ids = document
            .external_evidence
            .iter()
            .filter_map(|receipt| receipt.payload_artifact_id.clone())
            .collect();
        let live_artifact_ids =
            crate::tools::command_output_artifact::reconcile_evidence_artifact_protection(
                &codex_home,
                &thread_id_text,
                &referenced_artifact_ids,
            )
            .await;
        let repaired_snapshot = {
            let mut guard = ledger.document.lock().await;
            if let Some(document) = guard.as_mut() {
                let before = document.external_evidence.len();
                document.external_evidence.retain(|receipt| {
                    receipt
                        .payload_artifact_id
                        .as_ref()
                        .is_none_or(|artifact_id| live_artifact_ids.contains(artifact_id))
                });
                let removed = before.saturating_sub(document.external_evidence.len());
                if removed == 0 {
                    None
                } else {
                    warn!(
                        "removed {removed} external evidence receipt(s) whose payload artifacts were missing or invalid"
                    );
                    document.updated_at = timestamp();
                    document.revision = document.revision.saturating_add(1);
                    Some(document.clone())
                }
            } else {
                None
            }
        };
        if let Some(repaired_snapshot) = repaired_snapshot
            && ledger.persist_document(&repaired_snapshot).await != PersistOutcome::Persisted
        {
            warn!("failed to persist repaired external evidence receipts");
        }
        ledger
    }

    pub(crate) fn disabled() -> Self {
        Self {
            mode: TaskEvidenceMode::Disabled,
            codex_home: None,
            thread_id: None,
            evidence_path: None,
            repo_root: None,
            document: Arc::new(Mutex::new(None)),
            persistence_gate: Arc::new(Semaphore::new(1)),
            external_evidence_gate: Arc::new(Semaphore::new(1)),
            freshness_gate: Arc::new(Semaphore::new(1)),
            freshness_state: Arc::new(std::sync::Mutex::new(FreshnessState::default())),
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            source_capture_failed: Arc::new(AtomicBool::new(false)),
            desktop_activation_gate: Arc::new(Semaphore::new(1)),
            desktop_activation_runtime: Arc::new(std::sync::Mutex::new(
                DesktopActivationRuntimeState::default(),
            )),
            #[cfg(test)]
            persistence_test_control: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn desktop_activation_runtime_snapshot(&self) -> DesktopActivationRuntimeSnapshot {
        self.desktop_activation_runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    pub(crate) async fn desktop_activation_obligation(
        &self,
    ) -> Option<DesktopActivationObligation> {
        let runtime = self.desktop_activation_runtime_snapshot();
        let document = self.document.lock().await;
        let document = document.as_ref()?;
        let obligation = current_desktop_activation_obligation(document)?;
        if document
            .desktop_activation_receipt
            .as_ref()
            .is_some_and(|receipt| {
                desktop_activation_receipt_is_complete(receipt, &runtime, document)
            })
        {
            return None;
        }
        Some(obligation)
    }

    pub(crate) async fn issue_desktop_activation_challenge(
        &self,
        evidence: AuthoritativeDesktopInstallEvidence,
        bootstrap_consumed_at: String,
    ) -> Result<DesktopActivationChallenge, DesktopActivationVerificationError> {
        let _activation_permit = Arc::clone(&self.desktop_activation_gate)
            .acquire_owned()
            .await
            .map_err(|_| DesktopActivationVerificationError::PersistenceFailed)?;
        let runtime_snapshot = self.desktop_activation_runtime_snapshot();
        let (obligation, last_mutation_at) = {
            let document = self.document.lock().await;
            let document = document
                .as_ref()
                .ok_or(DesktopActivationVerificationError::ActivationObligationChanged)?;
            if document
                .desktop_activation_receipt
                .as_ref()
                .is_some_and(|receipt| {
                    desktop_activation_receipt_is_complete(receipt, &runtime_snapshot, document)
                })
            {
                return Err(DesktopActivationVerificationError::ActivationObligationChanged);
            }
            (
                current_desktop_activation_obligation(document)
                    .ok_or(DesktopActivationVerificationError::ActivationObligationChanged)?,
                document.last_mutation_at.clone(),
            )
        };
        let now = Utc::now();
        validate_authoritative_desktop_install_evidence(
            &evidence,
            "inherited-bootstrap-handle-v1",
            &evidence.publisher_evidence_id,
            &obligation,
            now,
        )?;
        if last_mutation_at
            .as_deref()
            .and_then(parse_desktop_timestamp)
            .is_some_and(|mutated_at| {
                parse_desktop_timestamp(&evidence.issued_at)
                    .is_none_or(|installed_at| installed_at < mutated_at)
            })
        {
            return Err(DesktopActivationVerificationError::AuthoritativeEvidenceStale);
        }
        let running_process = verified_desktop_running_process(&evidence).await?;
        {
            let document = self.document.lock().await;
            if document
                .as_ref()
                .and_then(current_desktop_activation_obligation)
                .as_ref()
                != Some(&obligation)
            {
                return Err(DesktopActivationVerificationError::ActivationObligationChanged);
            }
        }
        let mut runtime = self
            .desktop_activation_runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runtime.install_evidence_source =
            DesktopInstallEvidenceSource::AuthenticatedHostBootstrap {
                channel_identity: "inherited-bootstrap-handle-v1".to_string(),
                peer_identity: evidence.publisher_evidence_id.clone(),
                evidence: Box::new(evidence),
            };
        let pending = runtime.prepare_challenge(
            &obligation,
            running_process,
            &uuid::Uuid::new_v4().to_string(),
            &bootstrap_consumed_at,
            Utc::now(),
        )?;
        Ok(desktop_activation_challenge_public(&pending))
    }

    pub(crate) async fn record_desktop_activation(
        &self,
        observation: DesktopActivationRecordObservation,
    ) -> Result<DesktopActivationRecordResult, DesktopActivationVerificationError> {
        let _activation_permit = Arc::clone(&self.desktop_activation_gate)
            .acquire_owned()
            .await
            .map_err(|_| DesktopActivationVerificationError::PersistenceFailed)?;
        let request_hash = canonical_hash(
            "KD4_DESKTOP_ACTIVATION_RECORD_REQUEST_V1",
            &serde_json::to_value(&observation).unwrap_or(Value::Null),
        );
        if let Some(result) =
            self.recorded_desktop_activation_result(&observation.challenge_id, &request_hash)?
        {
            return Ok(result);
        }
        let pending = {
            let runtime = self
                .desktop_activation_runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            runtime
                .pending_challenge
                .as_ref()
                .filter(|pending| pending.challenge_identity == observation.challenge_id)
                .cloned()
                .ok_or(DesktopActivationVerificationError::ChallengeMissingOrConsumed)?
        };
        if Instant::now() > pending.monotonic_deadline {
            return Err(DesktopActivationVerificationError::ChallengeExpired);
        }
        verify_pending_desktop_running_process(&pending).await?;

        for _ in 0..8 {
            let expected_revision = {
                let document_guard = self.document.lock().await;
                let document = document_guard
                    .as_ref()
                    .ok_or(DesktopActivationVerificationError::ActivationObligationChanged)?;
                let obligation = current_desktop_activation_obligation(document)
                    .ok_or(DesktopActivationVerificationError::ActivationObligationChanged)?;
                if obligation.thread_id != pending.thread_id
                    || obligation.evidence_epoch != pending.evidence_epoch
                    || obligation.implementation_identity != pending.implementation_identity_hash
                    || obligation.activation_obligation_identity
                        != pending.activation_obligation_identity
                {
                    return Err(DesktopActivationVerificationError::ActivationObligationChanged);
                }
                let runtime_snapshot = self.desktop_activation_runtime_snapshot();
                if document
                    .desktop_activation_receipt
                    .as_ref()
                    .is_some_and(|receipt| {
                        desktop_activation_receipt_is_complete(receipt, &runtime_snapshot, document)
                    })
                {
                    return Err(DesktopActivationVerificationError::ActivationObligationChanged);
                }
                document.revision
            };
            let mut runtime_candidate = self
                .desktop_activation_runtime
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(recorded) = runtime_candidate
                .recorded_challenges
                .get(&observation.challenge_id)
            {
                if recorded.request_hash != request_hash {
                    return Err(
                        DesktopActivationVerificationError::ChallengeAlreadyRecordedWithDifferentPayload,
                    );
                }
                return Ok(DesktopActivationRecordResult {
                    challenge_id: observation.challenge_id,
                    recorded_at: recorded.recorded_at.clone(),
                    already_recorded: true,
                });
            }
            let acknowledgement = DesktopActivationAcknowledgement {
                challenge_identity: observation.challenge_id.clone(),
                authenticated_host_channel_identity: pending
                    .authenticated_host_channel_identity
                    .clone(),
                initialized_process_id: pending.running_process.process_id,
                initialized_process_identity: pending.running_process.process_identity.clone(),
                desktop_process_id: observation.desktop_process_id,
                desktop_process_identity: canonical_hash(
                    "KD4_DESKTOP_PROCESS_OBSERVATION_V1",
                    &serde_json::json!({
                        "processId": observation.desktop_process_id,
                        "path": observation.desktop_executable_path,
                    }),
                ),
                desktop_executable_path: observation.desktop_executable_path.clone(),
                initialization_observation_identity: observation
                    .initialization_observation_identity
                    .clone(),
                observed_at: observation.observation_timestamp.clone(),
            };
            let receipt = runtime_candidate.complete_challenge(acknowledgement, Utc::now())?;
            let result = DesktopActivationRecordResult {
                challenge_id: observation.challenge_id.clone(),
                recorded_at: receipt.activation_timestamp.clone(),
                already_recorded: false,
            };
            runtime_candidate.recorded_challenges.insert(
                observation.challenge_id.clone(),
                RecordedDesktopActivationChallenge {
                    request_hash: request_hash.clone(),
                    receipt: receipt.clone(),
                    recorded_at: result.recorded_at.clone(),
                },
            );
            while runtime_candidate.recorded_challenges.len() > 32 {
                let Some(first) = runtime_candidate.recorded_challenges.keys().next().cloned()
                else {
                    break;
                };
                runtime_candidate.recorded_challenges.remove(&first);
            }
            let committed_runtime = Arc::clone(&self.desktop_activation_runtime);
            let receipt_for_document = receipt.clone();
            let pending_for_document = pending.clone();
            let transition = self
                .atomic_review_update_with_commit(
                    expected_revision,
                    None,
                    None,
                    move |document| {
                        let Some(obligation) = current_desktop_activation_obligation(document)
                        else {
                            return false;
                        };
                        if obligation.thread_id != pending_for_document.thread_id
                            || obligation.evidence_epoch != pending_for_document.evidence_epoch
                            || obligation.implementation_identity
                                != pending_for_document.implementation_identity_hash
                            || obligation.activation_obligation_identity
                                != pending_for_document.activation_obligation_identity
                        {
                            return false;
                        }
                        document.desktop_activation_receipt = Some(receipt_for_document);
                        document.updated_at = timestamp();
                        document.completion = None;
                        true
                    },
                    move || {
                        *committed_runtime
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = runtime_candidate;
                    },
                )
                .await;
            match transition {
                AtomicReviewTransition::Persisted(true) => return Ok(result),
                AtomicReviewTransition::Persisted(false) => {
                    return Err(DesktopActivationVerificationError::ActivationObligationChanged);
                }
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => {
                    return Err(DesktopActivationVerificationError::PersistenceFailed);
                }
            }
        }
        Err(DesktopActivationVerificationError::PersistenceFailed)
    }

    fn recorded_desktop_activation_result(
        &self,
        challenge_id: &str,
        request_hash: &str,
    ) -> Result<Option<DesktopActivationRecordResult>, DesktopActivationVerificationError> {
        let runtime = self
            .desktop_activation_runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(recorded) = runtime.recorded_challenges.get(challenge_id) else {
            return Ok(None);
        };
        if recorded.request_hash != request_hash {
            return Err(
                DesktopActivationVerificationError::ChallengeAlreadyRecordedWithDifferentPayload,
            );
        }
        Ok(Some(DesktopActivationRecordResult {
            challenge_id: challenge_id.to_string(),
            recorded_at: recorded.recorded_at.clone(),
            already_recorded: true,
        }))
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> TaskEvidenceMode {
        self.mode
    }

    pub(crate) fn allows_kd4_completion(&self) -> bool {
        self.mode == TaskEvidenceMode::Kd4Completion
    }

    /// Atomically establishes the immutable terminal event and coordinates at-least-once live
    /// delivery. An existing identical record is returned for replay; a conflicting record is
    /// never replaced.
    pub(crate) async fn commit_terminal_decision_and_claim(
        &self,
        claim: TerminalDecisionClaim,
    ) -> TerminalClaimResult {
        if !claim.authoritative_event.is_self_consistent() {
            return TerminalClaimResult::Failed;
        }
        if !self.allows_kd4_completion() {
            return TerminalClaimResult::Claimed(claim.authoritative_event);
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return TerminalClaimResult::Failed;
            };
            let terminal_identity = claim.authoritative_event.terminal_identity.clone();
            let candidate_fingerprint = claim.authoritative_event.fingerprint.clone();
            let candidate_claim = claim.clone();
            let transition = self
                .atomic_review_update(expected_revision, None, None, |document| {
                    if let Some(receipt) =
                        document.terminalization_receipts.iter().find(|receipt| {
                            receipt.terminal_identity == terminal_identity
                                && receipt.delivery_state.is_authoritative_claim()
                        })
                    {
                        return match receipt.validated_authoritative_event().cloned() {
                            Some(authoritative)
                                if authoritative.fingerprint == candidate_fingerprint =>
                            {
                                TerminalClaimMutation::Existing(authoritative)
                            }
                            Some(authoritative) => {
                                TerminalClaimMutation::Conflict(Some(authoritative))
                            }
                            _ => TerminalClaimMutation::Conflict(None),
                        };
                    }
                    let now = timestamp();
                    let authoritative_event = candidate_claim.authoritative_event;
                    document
                        .terminalization_receipts
                        .push(TerminalizationReceipt {
                            terminal_identity: authoritative_event.terminal_identity.clone(),
                            durable_outcome: authoritative_event.semantic_outcome.clone(),
                            authoritative_event: Some(authoritative_event),
                            delivery_state: TerminalDeliveryState::Claimed,
                            app_server_acknowledged: false,
                            runtime_status_converged: false,
                            rollout_mirrored: false,
                            parent_notification_completed: false,
                            post_terminal_cleanup_completed: false,
                            active_turn_detached: false,
                            terminal_interaction_released: false,
                            deadline_exhausted_phase: candidate_claim.deadline_exhausted_phase,
                            mutation_quiescent: candidate_claim.mutation_quiescent,
                            durable_success_established: candidate_claim
                                .durable_success_established,
                            retained_ownership: candidate_claim.retained_ownership,
                            recovery_state: TerminalRecoveryState::Pending,
                            phase_timings_ns: candidate_claim.phase_timings_ns,
                            terminalization: None,
                            recorded_at: now.clone(),
                            updated_at: now,
                        });
                    trim_to_last(
                        &mut document.terminalization_receipts,
                        MAX_TERMINALIZATION_RECEIPTS,
                    );
                    document.updated_at = timestamp();
                    TerminalClaimMutation::Inserted
                })
                .await;
            match transition {
                AtomicReviewTransition::Persisted(TerminalClaimMutation::Inserted) => {
                    return TerminalClaimResult::Claimed(claim.authoritative_event);
                }
                AtomicReviewTransition::Persisted(TerminalClaimMutation::Existing(event)) => {
                    return TerminalClaimResult::AlreadyClaimed(event);
                }
                AtomicReviewTransition::Persisted(TerminalClaimMutation::Conflict(
                    authoritative,
                )) => {
                    return TerminalClaimResult::Conflict {
                        authoritative,
                        candidate_fingerprint,
                    };
                }
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => {
                    return self
                        .reconcile_terminal_claim_from_disk(&claim.authoritative_event)
                        .await
                        .unwrap_or(TerminalClaimResult::Failed);
                }
            }
        }
        self.reconcile_terminal_claim_from_disk(&claim.authoritative_event)
            .await
            .unwrap_or(TerminalClaimResult::Failed)
    }

    /// A replace/fsync failure can be ambiguous: the bytes may already be the durable file even
    /// though the writer returned an error. Re-read the authoritative store before permitting a
    /// caller to establish a different terminal event elsewhere.
    async fn reconcile_terminal_claim_from_disk(
        &self,
        candidate: &AuthoritativeTerminalEventV1,
    ) -> Option<TerminalClaimResult> {
        let path = self.evidence_path.as_ref()?;
        let bytes = tokio::fs::read(path).await.ok()?;
        let document = serde_json::from_slice::<TaskEvidenceDocument>(&bytes).ok()?;
        let receipt = document.terminalization_receipts.iter().find(|receipt| {
            receipt.terminal_identity == candidate.terminal_identity
                && receipt.delivery_state.is_authoritative_claim()
        })?;
        let result = match receipt.validated_authoritative_event().cloned() {
            Some(authoritative) if authoritative.fingerprint == candidate.fingerprint => {
                TerminalClaimResult::AlreadyClaimed(authoritative)
            }
            Some(authoritative) => TerminalClaimResult::Conflict {
                authoritative: Some(authoritative),
                candidate_fingerprint: candidate.fingerprint.clone(),
            },
            _ => TerminalClaimResult::Conflict {
                authoritative: None,
                candidate_fingerprint: candidate.fingerprint.clone(),
            },
        };
        self.last_persisted_revision
            .fetch_max(document.revision, Ordering::AcqRel);
        let mut guard = self.document.lock().await;
        if guard
            .as_ref()
            .is_none_or(|current| current.revision <= document.revision)
        {
            *guard = Some(document);
        }
        Some(result)
    }

    /// Durably records independently convergent terminal delivery and post-terminal effects.
    pub(crate) async fn update_terminal_interaction(
        &self,
        update: TerminalInteractionUpdate,
    ) -> bool {
        if !self.allows_kd4_completion() {
            return true;
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return false;
            };
            let terminal_identity = update.terminal_identity.clone();
            let candidate_update = update.clone();
            let transition = self
                .atomic_review_update(expected_revision, None, None, |document| {
                    let Some(receipt) = document
                        .terminalization_receipts
                        .iter_mut()
                        .find(|receipt| receipt.terminal_identity == terminal_identity)
                    else {
                        return false;
                    };
                    if !receipt.delivery_state.is_authoritative_claim() {
                        return false;
                    }
                    if receipt.delivery_state != TerminalDeliveryState::Delivered
                        && candidate_update.delivery_state != TerminalDeliveryState::Claimed
                    {
                        receipt.delivery_state = candidate_update.delivery_state;
                    }
                    receipt.app_server_acknowledged |= candidate_update.app_server_acknowledged;
                    receipt.runtime_status_converged |= candidate_update.runtime_status_converged;
                    receipt.rollout_mirrored |= candidate_update.rollout_mirrored;
                    receipt.parent_notification_completed |=
                        candidate_update.parent_notification_completed;
                    receipt.post_terminal_cleanup_completed |=
                        candidate_update.post_terminal_cleanup_completed;
                    receipt.active_turn_detached |= candidate_update.active_turn_detached;
                    receipt.terminal_interaction_released |=
                        candidate_update.terminal_interaction_released;
                    // Once recovery has completed, unrelated late cleanup updates must not
                    // erase that fact. A subsequent process reload can still mark an
                    // incomplete receipt Pending before replay begins.
                    if receipt.recovery_state != TerminalRecoveryState::Recovered {
                        receipt.recovery_state = candidate_update.recovery_state;
                    }
                    for (phase, duration) in candidate_update.phase_timings_ns {
                        receipt.phase_timings_ns.insert(phase, duration);
                    }
                    if let Some(terminalization) = candidate_update.terminalization {
                        receipt.terminalization = Some(terminalization);
                    }
                    receipt.updated_at = timestamp();
                    document.updated_at = timestamp();
                    true
                })
                .await;
            match transition {
                AtomicReviewTransition::Persisted(updated) => return updated,
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return false,
            }
        }
        false
    }

    pub(crate) async fn authoritative_terminal_event(
        &self,
        terminal_identity: &str,
    ) -> Option<AuthoritativeTerminalEventV1> {
        self.document
            .lock()
            .await
            .as_ref()?
            .terminalization_receipts
            .iter()
            .find(|receipt| receipt.terminal_identity == terminal_identity)
            .and_then(|receipt| receipt.validated_authoritative_event().cloned())
    }

    pub(crate) async fn pending_authoritative_terminal_events(
        &self,
    ) -> Vec<AuthoritativeTerminalEventV1> {
        self.document
            .lock()
            .await
            .as_ref()
            .map(|document| {
                document
                    .terminalization_receipts
                    .iter()
                    .filter(|receipt| {
                        receipt.delivery_state.is_authoritative_claim()
                            && (!receipt.app_server_acknowledged
                                || !receipt.runtime_status_converged
                                || !receipt.rollout_mirrored
                                || !receipt.parent_notification_completed
                                || !receipt.post_terminal_cleanup_completed
                                || !receipt.active_turn_detached
                                || !receipt.terminal_interaction_released)
                    })
                    .filter_map(|receipt| receipt.validated_authoritative_event().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn acknowledge_terminal_event(
        &self,
        terminal_identity: &str,
        fingerprint: &str,
    ) -> bool {
        if !self.allows_kd4_completion() {
            return true;
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return false;
            };
            let terminal_identity = terminal_identity.to_string();
            let fingerprint = fingerprint.to_string();
            let transition = self
                .atomic_review_update(expected_revision, None, None, |document| {
                    let Some(receipt) = document
                        .terminalization_receipts
                        .iter_mut()
                        .find(|receipt| receipt.terminal_identity == terminal_identity)
                    else {
                        return false;
                    };
                    let Some(authoritative) = receipt.validated_authoritative_event() else {
                        return false;
                    };
                    if authoritative.fingerprint != fingerprint {
                        return false;
                    }
                    receipt.app_server_acknowledged = true;
                    receipt.updated_at = timestamp();
                    document.updated_at = timestamp();
                    true
                })
                .await;
            match transition {
                AtomicReviewTransition::Persisted(acknowledged) => return acknowledged,
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return false,
            }
        }
        false
    }

    pub(crate) async fn terminalization_receipt_snapshot(
        &self,
        terminal_identity: &str,
    ) -> Option<TerminalizationReceiptSnapshot> {
        self.document
            .lock()
            .await
            .as_ref()?
            .terminalization_receipts
            .iter()
            .find(|receipt| receipt.terminal_identity == terminal_identity)
            .and_then(|receipt| {
                Some(TerminalizationReceiptSnapshot {
                    terminal_identity: receipt.terminal_identity.clone(),
                    terminalization: receipt.terminalization.clone()?,
                    delivery_state: receipt.delivery_state,
                    active_turn_detached: receipt.active_turn_detached,
                    terminal_interaction_released: receipt.terminal_interaction_released,
                    recovery_state: receipt.recovery_state,
                    deadline_exhausted_phase: receipt.deadline_exhausted_phase.clone(),
                })
            })
    }

    #[cfg(test)]
    pub(crate) async fn terminalization_receipts_for_test(
        &self,
    ) -> Vec<(
        String,
        TerminalDeliveryState,
        bool,
        bool,
        TerminalRecoveryState,
    )> {
        self.document
            .lock()
            .await
            .as_ref()
            .map(|document| {
                document
                    .terminalization_receipts
                    .iter()
                    .map(|receipt| {
                        (
                            receipt.terminal_identity.clone(),
                            receipt.delivery_state,
                            receipt.active_turn_detached,
                            receipt.terminal_interaction_released,
                            receipt.recovery_state,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) async fn terminal_timing_receipt_for_test(
        &self,
        terminal_identity: &str,
    ) -> Option<TurnTimingTerminalization> {
        self.document
            .lock()
            .await
            .as_ref()?
            .terminalization_receipts
            .iter()
            .find(|receipt| receipt.terminal_identity == terminal_identity)?
            .terminalization
            .clone()
    }

    pub(crate) async fn mark_user_source_capture_failed(&self) {
        self.source_capture_failed.store(true, Ordering::Release);
        if !self.allows_kd4_completion() {
            return;
        }
        let Some((_, snapshot)) = self
            .update_document(|document| {
                let root_task_id = document.thread_id.clone();
                let ledger = document
                    .completion_review_v2
                    .get_or_insert_with(|| new_completion_review_ledger(&root_task_id));
                ledger.source_capture_failed = true;
                ledger.review_risk.unresolved = true;
                ledger.review_risk.resolved_at = None;
                document.completion = None;
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        if self.persist_document(&snapshot).await != PersistOutcome::Persisted {
            warn!("failed to durably persist user-source capture failure");
        }
    }

    pub(crate) fn user_source_capture_failed(&self) -> bool {
        self.source_capture_failed.load(Ordering::Acquire)
    }

    pub(crate) fn matches_repo_root(&self, candidate: &Path) -> bool {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        repository_roots_match(repo_root, candidate)
    }

    pub(crate) fn repository_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
    }

    pub(crate) async fn last_workspace_event_epoch(&self) -> u64 {
        let guard = self.document.lock().await;
        guard
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .map(|ledger| ledger.last_workspace_event_epoch)
            .unwrap_or_default()
    }

    pub(crate) async fn typed_assignment_baseline(&self) -> BTreeSet<String> {
        let guard = self.document.lock().await;
        guard
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .map(|ledger| ledger.typed_assignment_baseline.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn seed_workspace_event_baseline(
        &self,
        workspace_epoch: u64,
        typed_assignment_baseline: BTreeSet<String>,
    ) -> bool {
        if !self.allows_kd4_completion() {
            return true;
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return false;
            };
            let typed_assignment_baseline = typed_assignment_baseline.clone();
            match self
                .atomic_review_update(expected_revision, None, None, move |document| {
                    let Some(ledger) = document.completion_review_v2.as_ref() else {
                        return false;
                    };
                    if ledger.workspace_event_baseline_epoch == ledger.completion_epoch
                        && ledger.workspace_event_history_complete
                    {
                        return true;
                    }
                    let completion_epoch = ledger.completion_epoch;
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        return false;
                    };
                    ledger.last_workspace_event_epoch = workspace_epoch;
                    ledger.workspace_event_baseline_epoch = completion_epoch;
                    ledger.typed_assignment_baseline = typed_assignment_baseline;
                    ledger.attributed_workspace_events.clear();
                    ledger.workspace_event_history_complete = true;
                    ledger.workspace_proof_scope_identity.clear();
                    resolve_risk(document, "completion-review-workspace-baseline");
                    document.updated_at = timestamp();
                    true
                })
                .await
            {
                AtomicReviewTransition::Persisted(result) => return result,
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return false,
            }
        }
        false
    }

    pub(crate) async fn mark_workspace_event_baseline_failed(&self, reason: &str) {
        if !self.allows_kd4_completion() {
            return;
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return;
            };
            let reason = reason.to_string();
            match self
                .atomic_review_update(expected_revision, None, None, move |document| {
                    let epoch = document.evidence_epoch;
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: "completion-review-workspace-baseline".to_string(),
                            description: reason,
                            source: "workspace-event-baseline".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
                    document.completion = None;
                    document.updated_at = timestamp();
                })
                .await
            {
                AtomicReviewTransition::Persisted(()) => return,
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return,
            }
        }
    }

    pub(crate) async fn record_user_sources(
        &self,
        message_id: &str,
        content: &[UserInput],
    ) -> bool {
        if !self.allows_kd4_completion() {
            return false;
        }
        let mut source_materials = Vec::new();
        for (index, input) in content.iter().enumerate() {
            if let Some((kind, exact_material, availability)) = user_source_material(input).await {
                source_materials.push((index as u64, kind, exact_material, availability));
            }
        }
        if source_materials.is_empty() {
            return true;
        }

        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return false;
            };
            let outcome = self
                .atomic_review_update(expected_revision, None, None, |document| {
                    let root_task_id = document.thread_id.clone();
                    let ledger = document
                        .completion_review_v2
                        .get_or_insert_with(|| new_completion_review_ledger(&root_task_id));
                    if ledger
                        .active_review_cycle
                        .as_ref()
                        .is_some_and(|cycle| cycle.phase == CompletionReviewCyclePhase::Closed)
                    {
                        ledger.completion_epoch = ledger.completion_epoch.saturating_add(1);
                        ledger.manifest_revision = 0;
                        ledger.active_review_cycle = None;
                        ledger.attributed_workspace_events.clear();
                        ledger.source_capture_failed = false;
                        ledger.review_risk = CompletionReviewRisk {
                            unresolved: false,
                            cycle_id: None,
                            opened_at: None,
                            resolved_at: None,
                        };
                        document.host_mutation_revision = 0;
                        document.latest_file_hashes.clear();
                        document.latest_generated_artifact_hashes.clear();
                        document.plan.clear();
                        document.active_step_id = None;
                        document.planning = PlanningEvidenceState::default();
                        document.risks.clear();
                        document.completion = None;
                    }
                    let mut inserted = false;
                    let mut inserted_ids = BTreeSet::new();
                    for (content_ordinal, kind, exact_material, availability) in &source_materials {
                        let content_hash =
                            user_source_content_hash(*kind, exact_material, *availability);
                        let source_id = deterministic_source_id(
                            &ledger.root_task_id,
                            ledger.completion_epoch,
                            message_id,
                            *content_ordinal,
                            &content_hash,
                        );
                        if ledger.source_records.contains_key(&source_id) {
                            continue;
                        }
                        let source_ordinal = ledger.next_source_ordinal;
                        ledger.next_source_ordinal = ledger.next_source_ordinal.saturating_add(1);
                        ledger.source_records.insert(
                            source_id.clone(),
                            UserSourceRecord {
                                source_id: source_id.clone(),
                                message_id: message_id.to_string(),
                                source_kind: *kind,
                                content_hash,
                                source_ordinal,
                                content_ordinal: *content_ordinal,
                                exact_material: exact_material.clone(),
                                availability: *availability,
                                completion_epoch: ledger.completion_epoch,
                                introduced_manifest_revision: ledger
                                    .manifest_revision
                                    .saturating_add(1),
                            },
                        );
                        inserted_ids.insert(source_id);
                        inserted = true;
                    }
                    if !inserted {
                        return true;
                    }

                    let previous_mappings = active_source_mappings(ledger);
                    let previous_requirements = active_manifest(ledger)
                        .map(|manifest| manifest.requirements.clone())
                        .unwrap_or_default();
                    ledger.manifest_revision = ledger.manifest_revision.saturating_add(1);
                    let manifest_revision = ledger.manifest_revision;
                    let active_source_ids = ledger
                        .source_records
                        .values()
                        .filter(|source| source.completion_epoch == ledger.completion_epoch)
                        .map(|source| source.source_id.clone())
                        .collect::<Vec<_>>();
                    for source_id in active_source_ids {
                        let mapping = if inserted_ids.contains(&source_id) {
                            SourceMapping::PendingClassification
                        } else {
                            previous_mappings
                                .get(&source_id)
                                .cloned()
                                .unwrap_or(SourceMapping::PendingClassification)
                        };
                        ledger.mapping_revisions.push(SourceMappingRevision {
                            completion_epoch: ledger.completion_epoch,
                            manifest_revision,
                            source_id,
                            source_classification_contract_version: None,
                            relationship_resolver_contract_version: None,
                            mapping,
                        });
                    }
                    let manifest_hash =
                        requirement_manifest_hash(manifest_revision, &previous_requirements);
                    ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                        completion_epoch: ledger.completion_epoch,
                        manifest_revision,
                        manifest_hash,
                        requirements: previous_requirements,
                    });
                    let cycle_id = format!(
                        "cycle-{}-{}",
                        ledger.completion_epoch, ledger.manifest_revision
                    );
                    let parent_terminal_review_id = ledger
                        .receipts
                        .iter()
                        .rev()
                        .find(|receipt| {
                            receipt.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
                        })
                        .map(|receipt| receipt.review_id.clone());
                    ledger.active_review_cycle = Some(CompletionReviewCycle {
                        cycle_id,
                        manifest_revision,
                        parent_terminal_review_id,
                        superseded_review_id: None,
                        phase: CompletionReviewCyclePhase::ClassificationPending,
                        correction_consumed: false,
                        manifest_gap_reconstructed: false,
                        accepted_review_id: None,
                        accepted_dossier_snapshot_id: None,
                    });
                    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
                    document.desktop_activation_receipt = None;
                    document.completion = None;
                    document.updated_at = timestamp();
                    true
                })
                .await;
            match outcome {
                AtomicReviewTransition::Persisted(result) => {
                    let source_capture_failed = self
                        .document
                        .lock()
                        .await
                        .as_ref()
                        .and_then(|document| document.completion_review_v2.as_ref())
                        .is_some_and(|ledger| ledger.source_capture_failed);
                    self.source_capture_failed
                        .store(source_capture_failed, Ordering::Release);
                    return result;
                }
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return false,
            }
        }
        false
    }

    #[cfg(test)]
    pub(crate) async fn record_plan_update(&self, update: &UpdatePlanArgs) -> UpdatePlanArgs {
        self.record_planning_update(PlanningUpdateInput {
            explanation: update.explanation.clone(),
            plan: update.plan.clone(),
            ..PlanningUpdateInput::default()
        })
        .await
        .public_update
    }

    pub(crate) async fn record_planning_update(
        &self,
        mut update: PlanningUpdateInput,
    ) -> PlanUpdateOutcome {
        let requested_public = UpdatePlanArgs {
            explanation: update.explanation.clone(),
            plan: update.plan.clone(),
        };
        if !self.allows_kd4_completion() {
            return PlanUpdateOutcome {
                public_update: requested_public,
                effect: PlanUpdateEffect::NoOp,
                unfinished_mutation_obligation: None,
            };
        }
        for fact in &mut update.facts {
            let normalized = fact
                .depends_on_paths
                .iter()
                .map(|path| {
                    self.repo_root.as_ref().map_or_else(
                        || normalize_slashes(path),
                        |repo_root| normalize_input_path(repo_root, None, Path::new(path)),
                    )
                })
                .collect::<BTreeSet<_>>();
            fact.depends_on_paths = normalized.into_iter().collect();
            // Freshly submitted evidence is current at the plan update
            // boundary; callers cannot revive persisted stale state by setting
            // this implementation field directly.
            fact.dependencies_current = true;
        }
        // Only a model-authored transition of the currently active, explicitly
        // named work unit to Implemented establishes a batch boundary. The
        // edit path's automatic status promotion never passes through here.
        let acknowledgement_candidate = {
            let guard = self.document.lock().await;
            guard.as_ref().and_then(|document| {
                let active_step_id = document.active_step_id.as_ref()?;
                let item = update.plan.iter().find(|item| {
                    item.id.as_deref() == Some(active_step_id.as_str())
                        && item.status == StepStatus::Implemented
                })?;
                let route = item.validation_route.clone().or_else(|| {
                    document
                        .plan
                        .iter()
                        .find(|step| step.id == *active_step_id)
                        .and_then(|step| step.validation_route.clone())
                })?;
                Some((
                    active_step_id.clone(),
                    document.host_mutation_revision,
                    route,
                ))
            })
        };
        let acknowledgement =
            if let Some((step_id, implementation_revision, route)) = acknowledgement_candidate {
                let covered_paths = validation_route_covered_paths(&route);
                let mut covered_manifest = Vec::with_capacity(covered_paths.len());
                if let Some(repo_root) = self.repo_root.as_ref() {
                    for path in covered_paths {
                        covered_manifest.push(snapshot_file(repo_root, &path).await);
                    }
                }
                Some(ImplementationBatchAcknowledgement {
                    step_id,
                    implementation_revision,
                    acknowledged_at: timestamp(),
                    covered_manifest,
                })
            } else {
                None
            };
        let Some((response, snapshot)) = self
            .update_document(|document| {
                let was_unplanned = document.plan.is_empty()
                    && document.planning.work_unit.is_none()
                    && document.planning.facts.is_empty();
                let mut used_ids = BTreeSet::new();
                let mut material_plan_change = false;
                let mut status_change = false;
                let mut duplicate_explicit_ids = BTreeSet::new();
                let mut seen_explicit_ids = BTreeSet::new();
                let step_evidence = update
                    .step_evidence
                    .iter()
                    .map(|evidence| (evidence.step_id.as_str(), evidence))
                    .collect::<BTreeMap<_, _>>();

                if let Some(tier) = update.tier
                    && document.planning.tier != tier
                {
                    document.planning.tier = tier;
                    material_plan_change = true;
                }
                for fact in &update.facts {
                    if document.planning.facts.get(&fact.id) != Some(fact) {
                        document
                            .planning
                            .facts
                            .insert(fact.id.clone(), fact.clone());
                        material_plan_change = true;
                    }
                }
                for removal in &update.removed_facts {
                    if document.planning.facts.remove(&removal.id).is_some() {
                        document.planning.audit_history.push(PlanningAuditEntry {
                            kind: "fact_removed".to_string(),
                            id: removal.id.clone(),
                            reason: removal.reason.clone(),
                            revision: document.planning.material_revision.saturating_add(1),
                            recorded_at: timestamp(),
                        });
                        document.planning.counters.fact_removals =
                            document.planning.counters.fact_removals.saturating_add(1);
                        material_plan_change = true;
                    }
                }
                for removal in &update.removed_steps {
                    if let Some(index) = document.plan.iter().position(|step| step.id == removal.id)
                    {
                        let retired = document.plan.remove(index);
                        document.planning.audit_history.push(PlanningAuditEntry {
                            kind: "step_removed".to_string(),
                            id: retired.id,
                            reason: removal.reason.clone(),
                            revision: retired.revision,
                            recorded_at: timestamp(),
                        });
                        document.planning.counters.step_removals =
                            document.planning.counters.step_removals.saturating_add(1);
                        material_plan_change = true;
                    }
                }

                for (index, item) in update.plan.iter().enumerate() {
                    if let Some(id) = item.id.as_ref()
                        && !seen_explicit_ids.insert(id.clone())
                    {
                        duplicate_explicit_ids.insert(id.clone());
                    }
                    let id = effective_step_id(item, index, &mut used_ids);
                    let old = document.plan.iter().find(|step| step.id == id).cloned();
                    let evidence = step_evidence.get(id.as_str()).copied();
                    let requested_obligations = evidence.map(|evidence| {
                        evidence
                            .mutation_obligations
                            .clone()
                            .into_iter()
                            .map(MutationObligationState::from)
                            .collect::<Vec<_>>()
                    });
                    let mut candidate = EvidencePlanStep {
                        id: id.clone(),
                        revision: old.as_ref().map_or(1, |step| step.revision),
                        step: item.step.clone(),
                        status: normalize_requested_status(&item.status),
                        depends_on: item.depends_on.clone(),
                        acceptance_criteria: item.acceptance_criteria.clone(),
                        runtime_paths: item.runtime_paths.clone(),
                        generated_artifacts: item.generated_artifacts.clone(),
                        risks: item.risks.clone(),
                        requires_desktop_activation: item.requires_desktop_activation,
                        validation_route: if evidence.is_some_and(|evidence| {
                            evidence.validation_disposition
                                == Some(ValidationDisposition::NotRequired)
                        }) {
                            None
                        } else {
                            item.validation_route.clone().or_else(|| {
                                old.as_ref().and_then(|step| step.validation_route.clone())
                            })
                        },
                        external_validation_route: evidence
                            .and_then(|evidence| evidence.external_validation_route.clone())
                            .or_else(|| {
                                old.as_ref()
                                    .and_then(|step| step.external_validation_route.clone())
                            }),
                        validation_disposition: evidence
                            .and_then(|evidence| evidence.validation_disposition)
                            .unwrap_or_else(|| {
                                old.as_ref().map_or_else(
                                    || {
                                        if item.validation_route.is_some() {
                                            ValidationDisposition::Executable
                                        } else {
                                            ValidationDisposition::NotRequired
                                        }
                                    },
                                    |step| step.validation_disposition,
                                )
                            }),
                        source_owner: evidence
                            .and_then(|evidence| evidence.source_owner.clone())
                            .or_else(|| old.as_ref().and_then(|step| step.source_owner.clone())),
                        implementation_surfaces: evidence
                            .map(|evidence| evidence.implementation_surfaces.clone())
                            .or_else(|| {
                                old.as_ref()
                                    .map(|step| step.implementation_surfaces.clone())
                            })
                            .unwrap_or_default(),
                        mutation_obligations: requested_obligations
                            .or_else(|| old.as_ref().map(|step| step.mutation_obligations.clone()))
                            .unwrap_or_default(),
                        validation_receipt_id: old
                            .as_ref()
                            .and_then(|step| step.validation_receipt_id.clone()),
                        edit_paths: old
                            .as_ref()
                            .map_or_else(BTreeSet::new, |step| step.edit_paths.clone()),
                    };
                    let material_step_change = old.as_ref().is_none_or(|step| {
                        !step_materially_matches_item(step, item)
                            || !step_internal_structure_matches(step, &candidate)
                    });
                    if material_step_change {
                        if let Some(old) = old.as_ref() {
                            candidate.revision = old.revision.saturating_add(1);
                            document.planning.counters.step_revisions =
                                document.planning.counters.step_revisions.saturating_add(1);
                        }
                        candidate.edit_paths.clear();
                        candidate.validation_receipt_id = None;
                        for obligation in &mut candidate.mutation_obligations {
                            obligation.satisfied = false;
                            obligation.satisfied_paths.clear();
                        }
                        material_plan_change = true;
                    } else if old
                        .as_ref()
                        .is_some_and(|step| step.status != candidate.status)
                    {
                        status_change = true;
                    }
                    candidate.status = admissible_requested_status(&candidate);
                    if let Some(position) = document.plan.iter().position(|step| step.id == id) {
                        document.plan[position] = candidate;
                    } else {
                        document.plan.push(candidate);
                    }
                }

                if document.planning.tier == PlanningTier::Focused && document.plan.is_empty() {
                    let work_unit = ensure_focused_work_unit(document);
                    let before = work_unit.clone();
                    if update.source_owner.is_some() {
                        work_unit.source_owner.clone_from(&update.source_owner);
                    }
                    if !update.implementation_surfaces.is_empty() {
                        work_unit.implementation_surfaces = update
                            .implementation_surfaces
                            .iter()
                            .map(|path| normalize_slashes(path))
                            .collect();
                    }
                    if !update.acceptance_criteria.is_empty() {
                        work_unit.acceptance_criteria = update.acceptance_criteria.clone();
                    }
                    if !update.mutation_obligations.is_empty() {
                        work_unit.mutation_obligations = update
                            .mutation_obligations
                            .clone()
                            .into_iter()
                            .map(MutationObligationState::from)
                            .collect();
                    }
                    if let Some(disposition) = update.validation_disposition {
                        work_unit.validation_disposition = disposition;
                    }
                    if update.validation_route.is_some() {
                        work_unit
                            .validation_route
                            .clone_from(&update.validation_route);
                    }
                    if update.external_validation_route.is_some() {
                        work_unit
                            .external_validation_route
                            .clone_from(&update.external_validation_route);
                    }
                    material_plan_change |= before != *work_unit;
                }
                if material_plan_change {
                    invalidate_for_plan_change(document);
                    document.planning.material_revision =
                        document.planning.material_revision.saturating_add(1);
                }
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                rebuild_declared_requirements_and_risks(document);
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                if plan_is_terminally_acknowledged(document) {
                    resolve_recoverable_runtime_risks(document);
                }
                if let Some(acknowledgement) = acknowledgement.clone()
                    && acknowledgement.implementation_revision == document.host_mutation_revision
                    && document.plan.iter().any(|step| {
                        step.id == acknowledgement.step_id
                            && step.status == StepStatus::Implemented
                            && step.validation_route.is_some()
                    })
                {
                    document.batch_acknowledgement = Some(acknowledgement);
                }
                document.updated_at = timestamp();
                document.completion = None;
                let effect = if material_plan_change {
                    if was_unplanned {
                        document.planning.counters.initial_updates =
                            document.planning.counters.initial_updates.saturating_add(1);
                        PlanUpdateEffect::Initial
                    } else {
                        document.planning.counters.structural_revisions = document
                            .planning
                            .counters
                            .structural_revisions
                            .saturating_add(1);
                        PlanUpdateEffect::StructuralRevision
                    }
                } else if status_change {
                    document.planning.counters.status_only_updates = document
                        .planning
                        .counters
                        .status_only_updates
                        .saturating_add(1);
                    PlanUpdateEffect::StatusOnly
                } else {
                    document.planning.counters.no_op_updates =
                        document.planning.counters.no_op_updates.saturating_add(1);
                    PlanUpdateEffect::NoOp
                };
                trim_to_last(
                    &mut document.planning.audit_history,
                    MAX_PLANNING_AUDIT_ENTRIES,
                );
                PlanUpdateOutcome {
                    public_update: UpdatePlanArgs {
                        explanation: update.explanation.clone(),
                        plan: document.plan.iter().map(plan_item_from_evidence).collect(),
                    },
                    effect,
                    unfinished_mutation_obligation: Some(has_unfinished_mutation_obligation(
                        document,
                    )),
                }
            })
            .await
        else {
            return PlanUpdateOutcome {
                public_update: requested_public,
                effect: PlanUpdateEffect::NoOp,
                unfinished_mutation_obligation: None,
            };
        };
        self.persist_document(&snapshot).await;
        response
    }

    /// Returns a route only when the explicit batch acknowledgement is still
    /// authoritative. This intentionally does not infer completion from time or
    /// from an automatically promoted Implemented status.
    pub(crate) async fn auto_validation_candidate(&self) -> Option<AutoValidationCandidate> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let (candidate, covered_manifest) = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            let acknowledgement = document.batch_acknowledgement.as_ref()?;
            if acknowledgement.implementation_revision != document.host_mutation_revision {
                return None;
            }
            let step = document
                .plan
                .iter()
                .find(|step| step.id == acknowledgement.step_id)?;
            if step.status != StepStatus::Implemented
                || !implementation_dependencies_satisfied(document, step)
            {
                return None;
            }
            let route = step.validation_route.clone()?;
            let covered_paths = validation_route_covered_paths(&route);
            let repository_wide = covered_paths.is_empty();
            let has_relevant_pending_intent = document.edit_intents.iter().any(|intent| {
                intent.completed_at.is_none()
                    && (repository_wide
                        || intent.files.iter().any(|file| {
                            covered_paths
                                .iter()
                                .any(|covered| validation_paths_overlap(covered, &file.path))
                        }))
            });
            if has_relevant_pending_intent {
                return None;
            }
            let leaf_implementation_identities = route
                .leaves
                .iter()
                .map(|leaf| {
                    validation_leaf_implementation_identity(
                        acknowledgement.implementation_revision,
                        leaf,
                        &acknowledgement.covered_manifest,
                    )
                })
                .collect();
            (
                AutoValidationCandidate {
                    step_id: step.id.clone(),
                    step_revision: step.revision,
                    route,
                    implementation_revision: acknowledgement.implementation_revision,
                    implementation_identity: validation_implementation_identity(
                        acknowledgement.implementation_revision,
                        repository_wide,
                        &acknowledgement.covered_manifest,
                    ),
                    leaf_implementation_identities,
                    repository_wide,
                },
                acknowledgement.covered_manifest.clone(),
            )
        };
        if !candidate.repository_wide {
            let repo_root = self.repo_root.as_ref()?;
            for expected in &covered_manifest {
                if expected.read_error.is_some() {
                    return None;
                }
                if snapshot_file(repo_root, &expected.path).await != *expected {
                    return None;
                }
            }
        }
        Some(candidate)
    }

    pub(crate) async fn direct_validation_implementation_identity(
        &self,
        covered_paths: &[String],
    ) -> Result<String, String> {
        if covered_paths.is_empty() {
            return Err("direct validation must declare non-empty covered_paths".to_string());
        }
        let repo_root = self
            .repo_root
            .as_ref()
            .ok_or_else(|| "direct validation requires a repository root".to_string())?;
        let mut normalized = covered_paths
            .iter()
            .map(|path| normalize_slashes(path).trim_start_matches("./").to_string())
            .collect::<Vec<_>>();
        normalized.sort();
        normalized.dedup();
        let mut covered_manifest = Vec::with_capacity(normalized.len());
        for path in normalized {
            let snapshot = snapshot_file(repo_root, &path).await;
            if let Some(read_error) = snapshot.read_error.as_deref() {
                return Err(format!(
                    "direct validation could not snapshot covered path `{path}`: {read_error}"
                ));
            }
            covered_manifest.push(snapshot);
        }
        Ok(validation_implementation_identity(
            0,
            false,
            &covered_manifest,
        ))
    }

    #[cfg(test)]
    pub(crate) async fn record_edit_intent(&self, call_id: &str, cwd: &Path, paths: &[PathBuf]) {
        self.record_edit_intent_with_provenance(call_id, cwd, paths, None)
            .await;
    }

    pub(crate) async fn record_locked_user_decisions(
        &self,
        call_id: &str,
        turn_id: &str,
        questions: &[RequestUserInputQuestion],
        response: &RequestUserInputResponse,
    ) {
        if !self.allows_kd4_completion() || response.interrupted {
            return;
        }
        let decisions = questions
            .iter()
            .filter(|question| !question.is_secret)
            .filter_map(|question| {
                response.answers.get(&question.id).map(|answer| {
                    (
                        question.clone(),
                        answer.answers.clone(),
                        format!("{call_id}:{}", question.id),
                    )
                })
            })
            .collect::<Vec<_>>();
        if decisions.is_empty() {
            return;
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                for (question, answers, decision_id) in decisions {
                    let supersedes = document
                        .locked_user_decisions
                        .iter()
                        .rev()
                        .find(|decision| {
                            decision.question_id == question.id
                                && decision.header == question.header
                                && decision.question == question.question
                                && decision.decision_id != decision_id
                        })
                        .map(|decision| decision.decision_id.clone());
                    document
                        .locked_user_decisions
                        .retain(|decision| decision.decision_id != decision_id);
                    document.locked_user_decisions.push(LockedUserDecision {
                        decision_id,
                        call_id: call_id.to_string(),
                        turn_id: turn_id.to_string(),
                        question_id: question.id,
                        header: question.header,
                        question: question.question,
                        answers,
                        recorded_at: timestamp(),
                        supersedes,
                    });
                }
                trim_to_last(
                    &mut document.locked_user_decisions,
                    MAX_LOCKED_USER_DECISIONS,
                );
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    pub(crate) async fn record_edit_intent_with_provenance(
        &self,
        call_id: &str,
        cwd: &Path,
        paths: &[PathBuf],
        provenance: Option<&ChildEvidenceProvenance>,
    ) {
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let normalized_paths = paths
            .iter()
            .map(|path| normalize_input_path(repo_root, Some(cwd), path))
            .collect::<BTreeSet<_>>();
        let mut files = Vec::with_capacity(normalized_paths.len());
        for normalized in normalized_paths {
            files.push(snapshot_file(repo_root, &normalized).await);
        }
        let evidence_call_id = provenance
            .map(|value| format!("{}:{call_id}", value.source_thread_id))
            .unwrap_or_else(|| call_id.to_string());

        let Some((_, snapshot)) = self
            .update_document(|document| {
                let (step_id, step_revision, work_unit_id, attribution) =
                    current_action_attribution(document, "edit", &evidence_call_id);
                document
                    .edit_intents
                    .retain(|intent| intent.call_id != evidence_call_id);
                document.edit_intents.push(EditIntent {
                    call_id: evidence_call_id,
                    step_id,
                    step_revision,
                    work_unit_id,
                    attribution: Some(attribution),
                    started_at: timestamp(),
                    completed_at: None,
                    outcome: None,
                    files,
                    source_thread_id: provenance.map(|value| value.source_thread_id.clone()),
                    source_agent_path: provenance.map(|value| value.source_agent_path.clone()),
                });
                trim_to_last(&mut document.edit_intents, MAX_EDIT_RECEIPTS);
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    #[cfg(test)]
    pub(crate) async fn record_edit_result(&self, call_id: &str, outcome: &str) {
        self.record_edit_result_with_provenance(call_id, outcome, None)
            .await;
    }

    pub(crate) async fn record_edit_result_with_provenance(
        &self,
        call_id: &str,
        outcome: &str,
        provenance: Option<&ChildEvidenceProvenance>,
    ) {
        if self.mode == TaskEvidenceMode::EvidenceOnly {
            if outcome == "completed" {
                self.record_host_mutation().await;
            }
            return;
        }
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let evidence_call_id = provenance
            .map(|value| format!("{}:{call_id}", value.source_thread_id))
            .unwrap_or_else(|| call_id.to_string());
        let intent = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .and_then(|document| {
                    document
                        .edit_intents
                        .iter()
                        .find(|intent| intent.call_id == evidence_call_id)
                })
                .cloned()
        };
        let Some(intent) = intent else {
            return;
        };
        let mut transitions = Vec::with_capacity(intent.files.len());
        let mut after_snapshots = Vec::with_capacity(intent.files.len());
        for before in &intent.files {
            let after = snapshot_file(repo_root, &before.path).await;
            if before != &after || before.read_error.is_some() || after.read_error.is_some() {
                transitions.push(FileHashTransition {
                    path: before.path.clone(),
                    before_sha1: before.sha1.clone(),
                    after_sha1: after.sha1.clone(),
                    before_exists: before.exists,
                    after_exists: after.exists,
                    before_read_error: before.read_error.clone(),
                    after_read_error: after.read_error.clone(),
                });
            }
            after_snapshots.push(after);
        }
        let edit_succeeded = edit_outcome_succeeded(outcome);

        let Some((_, snapshot)) = self
            .update_document(|document| {
                if let Some(stored) = document
                    .edit_intents
                    .iter_mut()
                    .find(|stored| stored.call_id == evidence_call_id)
                {
                    stored.completed_at = Some(timestamp());
                    stored.outcome = Some(outcome.to_string());
                }
                if !transitions.is_empty() {
                    let changed_paths = transitions
                        .iter()
                        .map(|transition| transition.path.clone())
                        .collect::<BTreeSet<_>>();
                    invalidate_for_mutation(document, Some(&changed_paths));
                    let epoch = document.evidence_epoch;
                    let mut affected_steps = BTreeMap::<String, BTreeSet<String>>::new();
                    for transition in &transitions {
                        if let Some(step_id) = intent.step_id.as_ref()
                            && document.plan.iter().any(|step| &step.id == step_id)
                        {
                            affected_steps
                                .entry(step_id.clone())
                                .or_default()
                                .insert(transition.path.clone());
                        }
                        for step in &document.plan {
                            if step.edit_paths.contains(&transition.path) {
                                affected_steps
                                    .entry(step.id.clone())
                                    .or_default()
                                    .insert(transition.path.clone());
                            }
                        }
                    }
                    for step in &mut document.plan {
                        if let Some(paths) = affected_steps.get(&step.id) {
                            step.edit_paths.extend(paths.iter().cloned());
                            if edit_succeeded {
                                record_obligation_progress(&mut step.mutation_obligations, paths);
                            }
                            if edit_succeeded
                                && implementation_obligations_satisfied(
                                    &step.mutation_obligations,
                                )
                                && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                            {
                                step.status = StepStatus::Implemented;
                            }
                        }
                    }
                    if let Some(work_unit_id) = intent.work_unit_id.as_deref()
                        && let Some(work_unit) = document.planning.work_unit.as_mut()
                        && work_unit.id == work_unit_id
                        && edit_succeeded
                    {
                        if work_unit.mutation_obligations.is_empty() {
                            work_unit.mutation_obligations.push(MutationObligationState {
                                id: "focused-mutation".to_string(),
                                description: "focused atomic mutation".to_string(),
                                paths: changed_paths.iter().cloned().collect(),
                                satisfied_paths: BTreeSet::new(),
                                satisfied: false,
                            });
                        }
                        record_obligation_progress(
                            &mut work_unit.mutation_obligations,
                            &changed_paths,
                        );
                    }
                    if affected_steps.is_empty() && intent.work_unit_id.is_none() {
                        upsert_risk(
                            document,
                            EvidenceRisk {
                                id: format!("unassociated-edit-{evidence_call_id}"),
                                description: format!(
                                    "edit `{evidence_call_id}` changed files without an active plan step"
                                ),
                                source: "edit".to_string(),
                                blocking: false,
                                resolved: false,
                                epoch,
                            },
                        );
                    }
                    for after in after_snapshots {
                        if after.read_error.is_some() {
                            upsert_risk(document, unreadable_file_risk(&after.path, epoch, "edit"));
                        }
                        document
                            .latest_file_hashes
                            .insert(after.path.clone(), after);
                    }
                    let receipt_id =
                        next_receipt_id("edit", &mut document.next_edit_receipt_sequence);
                    document.edit_receipts.push(EditReceipt {
                        id: receipt_id,
                        call_id: evidence_call_id,
                        step_id: intent.step_id,
                        step_revision: intent.step_revision,
                        work_unit_id: intent.work_unit_id,
                        attribution: intent.attribution,
                        recorded_at: timestamp(),
                        epoch,
                        outcome: outcome.to_string(),
                        files: transitions,
                        source_thread_id: intent.source_thread_id,
                        source_agent_path: intent.source_agent_path,
                    });
                    trim_to_last(&mut document.edit_receipts, MAX_EDIT_RECEIPTS);
                }
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
    ) {
        self.record_command_with_provenance(
            command,
            cwd,
            exit_code,
            timed_out,
            duration_ms,
            possible_mutation,
            None,
        )
        .await;
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command_with_provenance(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
        provenance: Option<&ChildEvidenceProvenance>,
    ) {
        self.record_command_bound_with_provenance(
            command,
            cwd,
            exit_code,
            timed_out,
            duration_ms,
            possible_mutation,
            None,
            provenance,
            None,
        )
        .await;
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command_bound_with_provenance(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
        mutation_paths: Option<&BTreeSet<PathBuf>>,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
    ) {
        self.record_command_bound_with_validation_result(
            command,
            cwd,
            exit_code,
            timed_out,
            duration_ms,
            possible_mutation,
            mutation_paths,
            provenance,
            implementation_identity_hash,
            None,
            None,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command_bound_with_validation_result<M>(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        mutation: M,
        mutation_paths: Option<&BTreeSet<PathBuf>>,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
        validation_result: Option<ValidationResult>,
        bound_plan_step: Option<(&str, u64)>,
    ) where
        M: Into<CommandMutation>,
    {
        let mutation = mutation.into();
        let possible_mutation = mutation.may_have_mutated();
        let observed_mutation = matches!(&mutation, CommandMutation::KnownMutation { .. });
        let mutation_paths = mutation.paths().or(mutation_paths);
        if self.mode == TaskEvidenceMode::EvidenceOnly {
            if observed_mutation {
                self.record_host_mutation().await;
            }
            return;
        }
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return;
        }
        let command_succeeded = exit_code == 0 && !timed_out;
        let Some((_, snapshot)) = self
            .update_document(|document| {
                let action_id = command.join("\u{1f}");
                let (step_id, step_revision, work_unit_id, attribution) = bound_plan_step
                    .filter(|(step_id, step_revision)| {
                        document.plan.iter().any(|step| {
                            step.id == *step_id && step.revision == *step_revision
                        })
                    })
                    .map(|(step_id, step_revision)| {
                        (
                            Some(step_id.to_string()),
                            Some(step_revision),
                            None,
                            ActionAttributionKind::PlannedStep,
                        )
                    })
                    .unwrap_or_else(|| {
                        current_action_attribution(document, "command", &action_id)
                    });
                if observed_mutation {
                    let normalized_paths = mutation_paths.map(|paths| {
                        paths
                            .iter()
                            .map(|path| {
                                normalize_input_path(
                                    Path::new(&document.start.repository_root),
                                    None,
                                    path,
                                )
                            })
                            .collect::<BTreeSet<_>>()
                    });
                    invalidate_for_mutation(document, normalized_paths.as_ref());
                    if normalized_paths.is_none() {
                        let epoch = document.evidence_epoch;
                        upsert_risk(
                            document,
                            EvidenceRisk {
                                id: format!("unknown-command-mutation-{epoch}"),
                                description:
                                    "the workspace changed during a command without exact path/hash attribution"
                                        .to_string(),
                                source: "command".to_string(),
                                blocking: false,
                                resolved: false,
                                epoch,
                            },
                        );
                    }
                } else if matches!(&mutation, CommandMutation::Uncertain) {
                    let epoch = document.evidence_epoch;
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("uninspected-command-mutation-{epoch}"),
                            description:
                                "workspace state could not be inspected before and after an uncertain command; mutation was not observed"
                                    .to_string(),
                            source: "command_observation".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
                }
                let receipt_id = next_receipt_id(
                    "command",
                    &mut document.next_command_receipt_sequence,
                );
                let contract_binding = completion_contract_hashes(document, false);
                let receipt = CommandReceipt {
                    id: receipt_id,
                    recorded_at: timestamp(),
                    epoch: document.evidence_epoch,
                    step_id,
                    step_revision,
                    work_unit_id,
                    attribution: Some(attribution),
                    command: command.to_vec(),
                    cwd: cwd.to_string(),
                    exit_code,
                    timed_out,
                    duration_ms,
                    possible_mutation,
                    observed_mutation,
                    host_mutation_revision: Some(document.host_mutation_revision),
                    manifest_revision: contract_binding
                        .as_ref()
                        .map(|(revision, _, _)| *revision),
                    user_source_ledger_hash: contract_binding
                        .as_ref()
                        .map(|(_, source_hash, _)| source_hash.clone()),
                    requirement_manifest_hash: contract_binding
                        .map(|(_, _, manifest_hash)| manifest_hash),
                    implementation_identity_hash: implementation_identity_hash.map(str::to_string),
                    validation_result,
                    source_thread_id: provenance.map(|value| value.source_thread_id.clone()),
                    source_agent_path: provenance.map(|value| value.source_agent_path.clone()),
                };
                if command_succeeded && matches!(&mutation, CommandMutation::ReadOnly) {
                    accept_matching_command_proof(document, &receipt);
                }
                document.command_receipts.push(receipt);
                trim_to_last(&mut document.command_receipts, MAX_COMMAND_RECEIPTS);
                document.updated_at = timestamp();
                document.completion = None;
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    /// Returns a bounded semantic projection of durable task state for context checkpoints.
    /// Proof payloads and command arguments are deliberately excluded.
    pub(crate) async fn compaction_task_state(&self) -> Option<String> {
        if !self.allows_kd4_completion() {
            return None;
        }
        self.refresh_external_file_freshness().await;
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        task_is_tracked(document).then(|| render_compaction_task_state(document))
    }

    /// Returns a complete recovery projection for compaction hooks, including a
    /// deterministic fallback when no durable task evidence has been recorded.
    pub(crate) async fn compaction_recovery_summary(&self) -> String {
        if !self.allows_kd4_completion() {
            return empty_compaction_task_state();
        }
        self.refresh_external_file_freshness().await;
        let guard = self.document.lock().await;
        guard
            .as_ref()
            .filter(|document| task_is_tracked(document))
            .map(render_compaction_task_state)
            .unwrap_or_else(empty_compaction_task_state)
    }

    pub(crate) async fn finalization_advisory(&self) -> Option<String> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let desktop_activation = self.desktop_activation_runtime_snapshot();
        let (gate, latest_review_audit) = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            task_is_tracked(document).then(|| {
                let gate = derive_completion_gate(
                    document,
                    self.evidence_path.as_deref(),
                    &desktop_activation,
                );
                let latest_review_audit = document
                    .completion_review_receipts
                    .iter()
                    .rev()
                    .find(|receipt| {
                        receipt.evidence_epoch == document.evidence_epoch
                            && (receipt.failure_category.is_some()
                                || !receipt.finding_summary.is_empty())
                    })
                    .cloned();
                (gate, latest_review_audit)
            })?
        };
        if gate.status == TaskCompletionStatus::Passed {
            return None;
        }
        let reasons = gate.reasons.iter().take(2).cloned().collect::<Vec<_>>();
        let reason_summary = if reasons.is_empty() {
            "evidence is incomplete".to_string()
        } else {
            reasons.join("; ")
        };
        let remaining = gate.reasons.len().saturating_sub(reasons.len());
        let remaining = if remaining == 0 {
            String::new()
        } else {
            format!("; and {remaining} more")
        };
        let review_audit = latest_review_audit
            .map(|receipt| {
                let category = receipt
                    .failure_category
                    .as_deref()
                    .unwrap_or("unclassified");
                let findings = receipt
                    .finding_summary
                    .iter()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>();
                if findings.is_empty() {
                    format!(" Latest completion-review audit: {category}.")
                } else {
                    format!(
                        " Latest completion-review audit: {category}: {}.",
                        findings.join("; ")
                    )
                }
            })
            .unwrap_or_default();
        Some(format!(
            "KD4 task evidence is {status}: {reason_summary}{remaining}.{review_audit} Before sending a final answer, reconcile durable task state: close completed implementation obligations in the plan, or explicitly state that durable task state remains unresolved. Do not claim completion while active or pending implementation obligations remain.",
            status = completion_status_name(gate.status),
        ))
    }

    // These inputs are distinct evidence domains whose ordering is part of the review boundary.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn completion_review_dossier(
        &self,
        candidate_completion: Option<&str>,
        typed_mutation_identities: &[String],
        typed_evidence: &[String],
        authoritative_lens_facts: &ReviewLensSelectionFacts,
        authoritative_input_errors: &[String],
        typed_quiescent: bool,
        default_children_quiescent: bool,
    ) -> Option<CompletionReviewDossier> {
        if !self.allows_kd4_completion() {
            return None;
        }
        self.refresh_external_file_freshness().await;
        let source_capture_failed = self.user_source_capture_failed();
        let candidate_completion = candidate_completion.map(str::to_string);
        let mut authoritative_input_errors = authoritative_input_errors.to_vec();
        authoritative_input_errors.sort();
        authoritative_input_errors.dedup();
        let desktop_activation_runtime = self.desktop_activation_runtime_snapshot();
        let (
            document_revision,
            root_task_id,
            completion_epoch,
            manifest_revision,
            sources,
            source_mappings,
            source_classification_cache,
            source_classification_current,
            relationship_resolution_current,
            mappings_classified,
            requirements,
            user_source_ledger_hash,
            requirement_manifest_hash,
            implementation_identity_hash,
            dossier_snapshot_id,
            host_mutation_revision,
            has_task_attributed_mutations,
            evidence_gate,
            locally_obtainable_proof_routes,
            reviewer_visible_evidence,
            review_lens_selection_facts,
            correction_consumed,
            cycle_phase,
            active_cycle_id,
            cycle_parent_review_id,
            cycle_superseded_review_id,
            accepted_review_id,
            initial_review_id,
            initial_repair_instruction_hash,
            original_findings,
            manifest_gap_reconstructed,
            current_repair_snapshot,
            initial_repair_baseline,
            initial_repair_baseline_hash,
        ) = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            let ledger = document.completion_review_v2.as_ref()?;
            let mut sources = ledger
                .source_records
                .values()
                .filter(|source| source.completion_epoch == ledger.completion_epoch)
                .cloned()
                .collect::<Vec<_>>();
            sources.sort_by_key(|source| (source.source_ordinal, source.content_ordinal));
            let source_mapping_revisions = source_mapping_revisions_for(
                ledger,
                ledger.completion_epoch,
                ledger.manifest_revision,
            );
            let source_mappings = source_mapping_revisions
                .iter()
                .map(|(source_id, revision)| (source_id.clone(), revision.mapping.clone()))
                .collect::<BTreeMap<_, _>>();
            let source_classification_current = sources.iter().all(|source| {
                source_mapping_revisions
                    .get(&source.source_id)
                    .is_some_and(|revision| {
                        revision.source_classification_contract_version.as_deref()
                            == Some(SOURCE_CLASSIFICATION_CONTRACT_VERSION)
                            && !matches!(revision.mapping, SourceMapping::PendingClassification)
                    })
            });
            let relationship_resolution_current = sources.iter().all(|source| {
                source_mapping_revisions
                    .get(&source.source_id)
                    .is_some_and(|revision| {
                        revision.relationship_resolver_contract_version.as_deref()
                            == Some(RELATIONSHIP_RESOLVER_CONTRACT_VERSION)
                            && !matches!(revision.mapping, SourceMapping::PendingClassification)
                    })
            });
            let mappings_classified =
                source_classification_current && relationship_resolution_current;
            let source_classification_cache = document
                .source_classification_cache
                .iter()
                .filter(|entry| entry.contract_version == SOURCE_CLASSIFICATION_CONTRACT_VERSION)
                .map(|entry| (entry.key(), entry.classification.clone()))
                .collect::<BTreeMap<_, _>>();
            let requirements = active_manifest(ledger)
                .map(|manifest| manifest.requirements.clone())
                .unwrap_or_default();
            let user_source_ledger_hash = user_source_ledger_snapshot_hash(
                ledger,
                ledger.completion_epoch,
                ledger.manifest_revision,
                source_capture_failed,
            );
            let requirement_manifest_hash =
                requirement_manifest_hash(ledger.manifest_revision, &requirements);
            let mut review_lens_selection_facts = authoritative_lens_facts.clone();
            review_lens_selection_facts.task_mutation_paths.extend(
                document
                    .latest_file_hashes
                    .values()
                    .map(|snapshot| snapshot.path.clone()),
            );
            review_lens_selection_facts.child_mutation_paths.extend(
                ledger
                    .attributed_workspace_events
                    .iter()
                    .flat_map(|event| event.paths.iter().cloned()),
            );
            review_lens_selection_facts.plan_edit_paths.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.edit_paths.iter().cloned()),
            );
            review_lens_selection_facts.plan_runtime_paths.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.runtime_paths.iter().cloned()),
            );
            review_lens_selection_facts.risk_hints.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.risks.iter().cloned()),
            );
            review_lens_selection_facts.generated_artifacts.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.generated_artifacts.iter().cloned()),
            );
            review_lens_selection_facts.generated_artifacts.extend(
                document
                    .generated_artifact_requirements
                    .iter()
                    .filter_map(|artifact| artifact.path.clone()),
            );
            for values in [
                &mut review_lens_selection_facts.risk_hints,
                &mut review_lens_selection_facts.task_mutation_paths,
                &mut review_lens_selection_facts.child_mutation_paths,
                &mut review_lens_selection_facts.plan_edit_paths,
                &mut review_lens_selection_facts.plan_runtime_paths,
                &mut review_lens_selection_facts.surface_roles,
                &mut review_lens_selection_facts.validation_asset_paths,
                &mut review_lens_selection_facts.generated_artifacts,
            ] {
                values.sort();
                values.dedup();
            }
            let mut typed_mutation_identities = typed_mutation_identities.to_vec();
            typed_mutation_identities.sort();
            typed_mutation_identities.dedup();
            let mut path_hashes = document
                .latest_file_hashes
                .values()
                .map(|snapshot| {
                    serde_json::json!({
                        "kind": "task_attributed",
                        "path": normalize_path_for_identity(Path::new(&snapshot.path)),
                        "exists": snapshot.exists,
                        "contentHash": snapshot.sha1,
                        "readError": snapshot.read_error,
                    })
                })
                .chain(
                    document
                        .latest_generated_artifact_hashes
                        .values()
                        .map(|snapshot| {
                            serde_json::json!({
                                "kind": "generated_artifact",
                                "path": normalize_path_for_identity(Path::new(&snapshot.path)),
                                "exists": snapshot.exists,
                                "contentHash": snapshot.sha1,
                                "readError": snapshot.read_error,
                            })
                        }),
                )
                .collect::<Vec<_>>();
            path_hashes.sort_by_key(std::string::ToString::to_string);
            let mut default_child_mutation_identities = ledger
                .attributed_workspace_events
                .iter()
                .map(|event| {
                    serde_json::json!({
                        "workspaceId": event.workspace_id,
                        "epoch": event.epoch,
                        "actorId": event.actor_id,
                        "paths": event.paths,
                    })
                })
                .collect::<Vec<_>>();
            default_child_mutation_identities.sort_by_key(std::string::ToString::to_string);
            let has_task_attributed_mutations = document.host_mutation_revision > 0
                || !path_hashes.is_empty()
                || !default_child_mutation_identities.is_empty()
                || !typed_mutation_identities.is_empty();
            let implementation_identity_hash = canonical_hash(
                IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT,
                &serde_json::json!({
                    "rootTaskId": ledger.root_task_id,
                    "completionEpoch": ledger.completion_epoch,
                    "manifestRevision": ledger.manifest_revision,
                    "userSourceLedgerHash": user_source_ledger_hash,
                    "requirementManifestHash": requirement_manifest_hash,
                    "mutationRevision": document.host_mutation_revision,
                    "paths": path_hashes,
                    "defaultChildMutationIdentities": default_child_mutation_identities,
                    "typedMutationIdentities": typed_mutation_identities,
                }),
            );
            let mut proof_receipts = document
                .command_receipts
                .iter()
                .filter(|receipt| {
                    receipt.epoch == document.evidence_epoch
                        && receipt.host_mutation_revision == Some(document.host_mutation_revision)
                        && receipt.manifest_revision == Some(ledger.manifest_revision)
                        && receipt.user_source_ledger_hash.as_deref()
                            == Some(user_source_ledger_hash.as_str())
                        && receipt.requirement_manifest_hash.as_deref()
                            == Some(requirement_manifest_hash.as_str())
                        && (receipt.implementation_identity_hash.as_deref()
                            == Some(implementation_identity_hash.as_str())
                            || command_receipt_has_current_proof_identity(document, receipt))
                })
                .map(|receipt| {
                    serde_json::json!({
                        "id": receipt.id,
                        "command": receipt.command,
                        "cwd": normalize_path_for_identity(Path::new(&receipt.cwd)),
                        "exitCode": receipt.exit_code,
                        "timedOut": receipt.timed_out,
                        "possibleMutation": receipt.possible_mutation,
                        "sourceThreadId": receipt.source_thread_id,
                        "sourceAgentPath": receipt.source_agent_path,
                        "boundImplementationIdentity": receipt.implementation_identity_hash,
                    })
                })
                .collect::<Vec<_>>();
            proof_receipts.sort_by_key(std::string::ToString::to_string);
            let mut external_evidence = document
                .external_evidence
                .iter()
                .filter(|receipt| {
                    receipt.task_epoch == document.evidence_epoch
                        && receipt.host_mutation_revision == Some(document.host_mutation_revision)
                        && receipt.implementation_identity_hash.as_deref()
                            == Some(implementation_identity_hash.as_str())
                })
                .map(|receipt| {
                    serde_json::json!({
                        "id": receipt.id,
                        "resultHash": receipt.result_sha256,
                        "complete": evidence_completeness_name(receipt.payload_completeness),
                        "truncated": receipt.truncated,
                        "approximate": receipt.approximate,
                        "limitations": receipt.limitations,
                        "providerSnapshot": receipt.provider_snapshot,
                        "sourceThreadId": receipt.source_thread_id,
                        "sourceAgentPath": receipt.source_agent_path,
                        "boundImplementationIdentity": receipt.implementation_identity_hash,
                    })
                })
                .collect::<Vec<_>>();
            external_evidence.sort_by_key(std::string::ToString::to_string);
            let mut typed_evidence = typed_evidence.to_vec();
            typed_evidence.sort();
            typed_evidence.dedup();
            let evidence_gate = derive_completion_gate(
                document,
                self.evidence_path.as_deref(),
                &desktop_activation_runtime,
            );
            let locally_obtainable_proof_routes =
                completion_review_locally_obtainable_proof_routes(&evidence_gate);
            let desktop_activation =
                document
                    .desktop_activation_receipt
                    .as_ref()
                    .filter(|receipt| {
                        desktop_activation_receipt_is_complete(
                            receipt,
                            &desktop_activation_runtime,
                            document,
                        )
                    });
            let reviewer_visible_evidence = serde_json::json!({
                "implementationIdentity": implementation_identity_hash,
                "taskAttributedPaths": path_hashes,
                "defaultChildMutationIdentities": default_child_mutation_identities,
                "typedMutationIdentities": typed_mutation_identities,
                "proofReceipts": proof_receipts,
                "externalEvidence": external_evidence,
                "desktopActivation": desktop_activation,
                "plan": document.plan,
                "risks": document.risks,
                "evidenceGate": evidence_gate,
                "locallyObtainableProofRoutes": locally_obtainable_proof_routes,
                "typedEvidence": typed_evidence,
                "authoritativeInputErrors": authoritative_input_errors,
                "typedQuiescent": typed_quiescent,
                "defaultChildrenQuiescent": default_children_quiescent,
                "candidateCompletion": candidate_completion,
            });
            let dossier_snapshot_id = canonical_hash(
                DOSSIER_SNAPSHOT_CANONICAL_FORMAT,
                &reviewer_visible_evidence,
            );
            let cycle = ledger.active_review_cycle.as_ref();
            let current_repair_snapshot =
                current_repair_snapshot(document, &typed_mutation_identities);
            let initial_receipt = cycle.and_then(|cycle| {
                if matches!(
                    cycle.phase,
                    CompletionReviewCyclePhase::ClassificationPending
                        | CompletionReviewCyclePhase::InitialReviewPending
                ) {
                    return None;
                }
                let cycle_start = cycle
                    .parent_terminal_review_id
                    .as_ref()
                    .and_then(|parent_id| {
                        ledger
                            .receipts
                            .iter()
                            .position(|receipt| receipt.review_id == *parent_id)
                    })
                    .map_or(0, |parent_index| parent_index.saturating_add(1));
                ledger.receipts[cycle_start..].iter().rev().find(|receipt| {
                    receipt.attempt_kind == CompletionReviewAttemptKind::InitialReview
                        && receipt.requirement_manifest_hash == requirement_manifest_hash
                        && receipt.terminal_outcome.is_none()
                })
            });
            (
                document.revision,
                ledger.root_task_id.clone(),
                ledger.completion_epoch,
                ledger.manifest_revision,
                sources,
                source_mappings,
                source_classification_cache,
                source_classification_current,
                relationship_resolution_current,
                mappings_classified,
                requirements,
                user_source_ledger_hash,
                requirement_manifest_hash,
                implementation_identity_hash,
                dossier_snapshot_id,
                document.host_mutation_revision,
                has_task_attributed_mutations,
                evidence_gate,
                locally_obtainable_proof_routes,
                reviewer_visible_evidence,
                review_lens_selection_facts,
                cycle.is_some_and(|cycle| cycle.correction_consumed),
                cycle.map(|cycle| cycle.phase),
                cycle.map(|cycle| cycle.cycle_id.clone()),
                cycle.and_then(|cycle| cycle.parent_terminal_review_id.clone()),
                cycle.and_then(|cycle| cycle.superseded_review_id.clone()),
                cycle.and_then(|cycle| cycle.accepted_review_id.clone()),
                initial_receipt.map(|receipt| receipt.review_id.clone()),
                initial_receipt.and_then(|receipt| receipt.repair_instruction_hash.clone()),
                initial_receipt
                    .map(|receipt| receipt.findings.clone())
                    .unwrap_or_default(),
                cycle.is_some_and(|cycle| cycle.manifest_gap_reconstructed),
                current_repair_snapshot,
                initial_receipt.and_then(|receipt| receipt.repair_baseline.clone()),
                initial_receipt.and_then(|receipt| receipt.baseline_hash.clone()),
            )
        };
        let rereview_input = build_rereview_input(
            initial_repair_baseline.as_ref(),
            initial_repair_baseline_hash.as_deref(),
            initial_repair_instruction_hash.as_deref(),
            &original_findings,
            &current_repair_snapshot,
            &implementation_identity_hash,
            &user_source_ledger_hash,
            &requirement_manifest_hash,
        );
        Some(CompletionReviewDossier {
            document_revision,
            root_task_id,
            completion_epoch,
            manifest_revision,
            sources,
            source_mappings,
            source_classification_cache,
            source_classification_current,
            relationship_resolution_current,
            mappings_classified,
            source_capture_failed,
            requirements,
            user_source_ledger_hash,
            requirement_manifest_hash,
            implementation_identity_hash,
            dossier_snapshot_id,
            host_mutation_revision,
            has_task_attributed_mutations,
            evidence_gate,
            locally_obtainable_proof_routes,
            reviewer_visible_evidence,
            review_lens_selection_facts,
            authoritative_input_errors,
            typed_quiescent,
            default_children_quiescent,
            candidate_completion,
            correction_consumed,
            cycle_phase,
            active_cycle_id,
            cycle_parent_review_id,
            cycle_superseded_review_id,
            accepted_review_id,
            initial_review_id,
            initial_repair_instruction_hash,
            original_findings,
            manifest_gap_reconstructed,
            current_repair_snapshot,
            initial_repair_baseline,
            initial_repair_baseline_hash,
            rereview_input,
        })
    }

    pub(crate) async fn begin_completion_review_cycle(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<String> {
        self.atomic_review_update(dossier.document_revision, None, None, |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                unreachable!("V2 dossier requires a V2 ledger");
            };
            let cycle = ledger
                .active_review_cycle
                .get_or_insert_with(|| CompletionReviewCycle {
                    cycle_id: format!(
                        "cycle-{}-{}",
                        ledger.completion_epoch, ledger.manifest_revision
                    ),
                    manifest_revision: ledger.manifest_revision,
                    parent_terminal_review_id: ledger.last_terminal_closure.clone(),
                    superseded_review_id: None,
                    phase: if dossier.mappings_classified {
                        CompletionReviewCyclePhase::InitialReviewPending
                    } else {
                        CompletionReviewCyclePhase::ClassificationPending
                    },
                    correction_consumed: false,
                    manifest_gap_reconstructed: false,
                    accepted_review_id: None,
                    accepted_dossier_snapshot_id: None,
                });
            if cycle.manifest_revision != dossier.manifest_revision {
                cycle.manifest_revision = dossier.manifest_revision;
                cycle.phase = if dossier.mappings_classified {
                    CompletionReviewCyclePhase::InitialReviewPending
                } else {
                    CompletionReviewCyclePhase::ClassificationPending
                };
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
                cycle.manifest_gap_reconstructed = false;
                cycle.superseded_review_id = None;
            }
            ledger.review_risk = CompletionReviewRisk {
                unresolved: true,
                cycle_id: Some(cycle.cycle_id.clone()),
                opened_at: Some(timestamp()),
                resolved_at: None,
            };
            document.completion = None;
            document.updated_at = timestamp();
            cycle.cycle_id.clone()
        })
        .await
    }

    pub(crate) async fn preview_completion_review_id(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> Option<String> {
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        if document.revision != dossier.document_revision {
            return None;
        }
        let ledger = document.completion_review_v2.as_ref()?;
        Some(format!(
            "review-{}-{}-{}",
            ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
        ))
    }

    pub(crate) async fn record_completion_review_attempt_v2(
        &self,
        dossier: &CompletionReviewDossier,
        input: CompletionReviewAttemptInput,
    ) -> AtomicReviewTransition<RecordedReviewAttempt> {
        self.record_completion_review_attempt_v2_inner(dossier, input, None)
            .await
    }

    pub(crate) async fn record_completion_review_attempt_v2_with_materialization(
        &self,
        dossier: &CompletionReviewDossier,
        input: CompletionReviewAttemptInput,
        materialization: SourceMaterialization,
    ) -> AtomicReviewTransition<RecordedReviewAttempt> {
        self.record_completion_review_attempt_v2_inner(dossier, input, Some(materialization))
            .await
    }

    async fn record_completion_review_attempt_v2_inner(
        &self,
        dossier: &CompletionReviewDossier,
        input: CompletionReviewAttemptInput,
        gap_materialization: Option<SourceMaterialization>,
    ) -> AtomicReviewTransition<RecordedReviewAttempt> {
        if input.attempt_kind == CompletionReviewAttemptKind::TerminalClosure {
            return AtomicReviewTransition::Failed;
        }
        let reconstruct_manifest = !input.manifest_gaps.is_empty();
        let prepared_gap_materialization = match (reconstruct_manifest, gap_materialization) {
            (true, Some(materialization)) => {
                let Some(prepared) = prepare_source_materialization(dossier, materialization)
                else {
                    return AtomicReviewTransition::Failed;
                };
                if !prepared_materialization_covers_manifest_gaps(
                    dossier,
                    &input.manifest_gaps,
                    &prepared,
                ) {
                    return AtomicReviewTransition::Failed;
                }
                Some(prepared)
            }
            (false, None) => None,
            _ => return AtomicReviewTransition::Failed,
        };
        let expected_ordinals = (1..=input.findings.len() as u32).collect::<Vec<_>>();
        let ordinals = input
            .findings
            .iter()
            .map(|finding| finding.local_ordinal)
            .collect::<Vec<_>>();
        if ordinals != expected_ordinals
            || (input.review_clean && !input.findings.is_empty())
            || (input.repair_instruction.is_some() && input.repair_instruction_hash.is_some())
            || input
                .repair_instruction_hash
                .as_deref()
                .is_some_and(|hash| {
                    hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            || (reconstruct_manifest
                && (input.review_clean
                    || input.repair_instruction.is_some()
                    || input.infrastructure_outcome != "ok"
                    || input.terminal_outcome.is_some()))
            || input.infrastructure_outcome.trim().is_empty()
            || (matches!(
                input.attempt_kind,
                CompletionReviewAttemptKind::InitialReview | CompletionReviewAttemptKind::Rereview
            ) && (input.attempt_identity.trim().is_empty()
                || input.reviewer_contract_hash.trim().is_empty()))
            || !matches!(
                input.terminal_outcome.as_deref(),
                None | Some("partial") | Some("blocked")
            )
        {
            return AtomicReviewTransition::Failed;
        }
        let infrastructure_ok = input.infrastructure_outcome == "ok";
        if !infrastructure_ok
            && (input.review_clean
                || !input.findings.is_empty()
                || !input.dispositions.is_empty()
                || !input.manifest_gaps.is_empty()
                || input.terminal_outcome.as_deref() != Some("partial"))
        {
            return AtomicReviewTransition::Failed;
        }
        let valid_requirement_ids = dossier
            .requirements
            .iter()
            .map(|requirement| requirement.requirement_id.as_str())
            .collect::<BTreeSet<_>>();
        let active_requirement_ids = dossier
            .requirements
            .iter()
            .filter(|requirement| requirement.status == RequirementStatus::Active)
            .map(|requirement| requirement.requirement_id.as_str())
            .collect::<BTreeSet<_>>();
        if input.findings.iter().any(|finding| {
            let unique_requirement_ids = finding
                .requirement_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            unique_requirement_ids.len() != finding.requirement_ids.len()
                || (!unique_requirement_ids.is_empty()
                    && !unique_requirement_ids
                        .iter()
                        .any(|requirement_id| active_requirement_ids.contains(requirement_id)))
                || finding
                    .requirement_ids
                    .iter()
                    .any(|requirement_id| !valid_requirement_ids.contains(requirement_id.as_str()))
                || finding.lens.trim().is_empty()
                || finding.contract_surface.trim().is_empty()
                || finding.severity.trim().is_empty()
                || finding.evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.proof_route.trim().is_empty()
        }) {
            return AtomicReviewTransition::Failed;
        }
        let valid_dispositions = [
            "resolved",
            "rebuttal_accepted",
            "still_present",
            "insufficient_proof",
            "regressed",
        ];
        if input.dispositions.iter().any(|disposition| {
            disposition.finding_id.trim().is_empty()
                || disposition.evidence.trim().is_empty()
                || !valid_dispositions.contains(&disposition.disposition.as_str())
        }) {
            return AtomicReviewTransition::Failed;
        }
        let needs_correction = input.repair_instruction.is_some()
            || (input.attempt_kind == CompletionReviewAttemptKind::CorrectionEvidence
                && input.repair_instruction_hash.is_some());
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(cycle) = ledger.active_review_cycle.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision
                || cycle.manifest_revision != dossier.manifest_revision
            {
                return AtomicReviewTransition::Failed;
            }
            let valid_lifecycle = match input.attempt_kind {
                CompletionReviewAttemptKind::InitialReview => {
                    (cycle.phase == CompletionReviewCyclePhase::InitialReviewPending
                        || (!infrastructure_ok
                            && cycle.phase == CompletionReviewCyclePhase::ClassificationPending))
                        && input.parent_review_id == cycle.parent_terminal_review_id
                        && input.superseded_review_id == cycle.superseded_review_id
                        && input.dispositions.is_empty()
                        && input.repair_instruction_hash.is_none()
                }
                CompletionReviewAttemptKind::CorrectionEvidence => {
                    cycle.phase == CompletionReviewCyclePhase::CorrectionPending
                        && !cycle.correction_consumed
                        && input.superseded_review_id.is_none()
                        && input.parent_review_id == dossier.initial_review_id
                        && input.findings.is_empty()
                        && input.dispositions.is_empty()
                        && needs_correction
                        && input.repair_instruction.is_none()
                        && input.repair_instruction_hash == dossier.initial_repair_instruction_hash
                        && !input.review_clean
                }
                CompletionReviewAttemptKind::Rereview => {
                    cycle.phase == CompletionReviewCyclePhase::RereviewPending
                        && cycle.correction_consumed
                        && input.superseded_review_id.is_none()
                        && input.parent_review_id == dossier.initial_review_id
                        && input.repair_instruction_hash == dossier.initial_repair_instruction_hash
                        && input.repair_instruction.is_none()
                }
                CompletionReviewAttemptKind::TerminalClosure => false,
            };
            if !valid_lifecycle {
                return AtomicReviewTransition::Failed;
            }
            if input.attempt_kind == CompletionReviewAttemptKind::Rereview && infrastructure_ok {
                let expected = dossier
                    .original_findings
                    .iter()
                    .map(|finding| finding.finding_id.as_str())
                    .collect::<BTreeSet<_>>();
                let returned = input
                    .dispositions
                    .iter()
                    .map(|disposition| disposition.finding_id.as_str())
                    .collect::<BTreeSet<_>>();
                if returned.len() != input.dispositions.len() || returned != expected {
                    return AtomicReviewTransition::Failed;
                }
            }
        }
        if let Some(parent_review_id) = input.parent_review_id.as_deref() {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision
                || document.completion_review_v2.as_ref().is_none_or(|ledger| {
                    !ledger
                        .receipts
                        .iter()
                        .any(|receipt| receipt.review_id == parent_review_id)
                })
            {
                return AtomicReviewTransition::Failed;
            }
        }
        let repair_instruction_hash = input.repair_instruction.as_deref().map_or_else(
            || input.repair_instruction_hash.clone(),
            |instruction| {
                Some(canonical_hash(
                    REPAIR_INSTRUCTION_CANONICAL_FORMAT,
                    &serde_json::json!({ "instruction": instruction }),
                ))
            },
        );
        let repair_baseline_metadata = if input.attempt_kind
            == CompletionReviewAttemptKind::InitialReview
            && input.repair_instruction.is_some()
        {
            let preview_findings = input
                .findings
                .iter()
                .map(|finding| CompletionReviewFindingReceipt {
                    finding_id: format!("preview/F{}", finding.local_ordinal),
                    requirement_ids: finding.requirement_ids.clone(),
                    lens: finding.lens.clone(),
                    contract_surface: finding.contract_surface.clone(),
                    severity: finding.severity.clone(),
                    evidence: finding.evidence.clone(),
                    smallest_correction: finding.smallest_correction.clone(),
                    proof_route: finding.proof_route.clone(),
                })
                .collect::<Vec<_>>();
            bind_initial_repair_baseline_metadata(
                build_repair_baseline(dossier, &preview_findings),
                input.repair_instruction.as_deref(),
            )
        } else {
            None
        };
        let rereview_metadata = if input.attempt_kind == CompletionReviewAttemptKind::Rereview {
            let Some(rereview_input) = dossier.rereview_input.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if !validate_rereview_input(rereview_input, dossier) {
                return AtomicReviewTransition::Failed;
            }
            Some(rereview_input.clone())
        } else {
            None
        };
        let persisted_repair_baseline = repair_baseline_metadata
            .as_ref()
            .map(|(baseline, _)| baseline.clone());
        let persisted_baseline_hash = rereview_metadata
            .as_ref()
            .and_then(|metadata| metadata.baseline_hash.clone())
            .or_else(|| {
                repair_baseline_metadata
                    .as_ref()
                    .map(|(_, hash)| hash.clone())
            });
        let persisted_input_mode = rereview_metadata
            .as_ref()
            .map(|metadata| metadata.input_mode);
        let persisted_delta_hash = rereview_metadata
            .as_ref()
            .and_then(|metadata| metadata.delta_hash.clone());
        let persisted_rereview_delta = rereview_metadata
            .as_ref()
            .and_then(|metadata| metadata.delta.clone());
        let persisted_fallback_reasons = rereview_metadata
            .as_ref()
            .map(|metadata| metadata.fallback_reasons.clone())
            .unwrap_or_default();
        let persisted_candidate_identity = rereview_metadata
            .as_ref()
            .map(|metadata| metadata.candidate_implementation_identity.clone());
        let persisted_rereview_audit_hash = rereview_metadata.as_ref().map(rereview_audit_hash);
        let attempt_kind = input.attempt_kind;
        let parent_review_id = input.parent_review_id.clone();
        let superseded_review_id = input.superseded_review_id.clone();
        let dispositions = input.dispositions.clone();
        let manifest_gaps = input.manifest_gaps.clone();
        let infrastructure_outcome = input.infrastructure_outcome.clone();
        let terminal_outcome = input.terminal_outcome.clone();
        let review_clean = input.review_clean;
        let attempt_identity = input.attempt_identity.clone();
        let reviewer_contract_hash = input.reviewer_contract_hash.clone();
        let evidence_path = self
            .evidence_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision {
                return AtomicReviewTransition::Superseded;
            }
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if !dossier_sources_are_current(ledger, &dossier.sources) {
                return AtomicReviewTransition::Failed;
            }
        }
        let mut gap_persistence = prepared_gap_materialization.map(|prepared| {
            let PreparedSourceMaterialization {
                local_classifications,
                requirements,
                mappings,
            } = prepared;
            let replacement_cache_keys = local_classifications
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            let replacement_cache_entries = local_classifications
                .into_iter()
                .map(|(key, classification)| SourceClassificationCacheEntry {
                    contract_version: key.contract_version,
                    source_kind: key.source_kind,
                    content_hash: key.content_hash,
                    classification,
                })
                .collect::<Vec<_>>();
            (
                replacement_cache_keys,
                replacement_cache_entries,
                requirements,
                mappings,
            )
        });
        self.atomic_review_update(dossier.document_revision, None, None, move |document| {
            let current_validation_receipt_ids = reconstruct_manifest.then(|| {
                document
                    .command_receipts
                    .iter()
                    .filter(|receipt| command_receipt_has_current_proof_identity(document, receipt))
                    .map(|receipt| receipt.id.clone())
                    .collect::<BTreeSet<_>>()
            });
            if let Some((replacement_cache_keys, replacement_cache_entries, _, _)) =
                gap_persistence.as_mut()
            {
                document
                    .source_classification_cache
                    .retain(|entry| !replacement_cache_keys.contains(&entry.key()));
                document
                    .source_classification_cache
                    .append(replacement_cache_entries);
            }
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                unreachable!("V2 dossier requires a V2 ledger");
            };
            let review_id = format!(
                "review-{}-{}-{}",
                ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
            );
            ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
            let findings = input
                .findings
                .into_iter()
                .map(|finding| CompletionReviewFindingReceipt {
                    finding_id: format!("{review_id}/F{}", finding.local_ordinal),
                    requirement_ids: finding.requirement_ids,
                    lens: finding.lens,
                    contract_surface: finding.contract_surface,
                    severity: finding.severity,
                    evidence: finding.evidence,
                    smallest_correction: finding.smallest_correction,
                    proof_route: finding.proof_route,
                })
                .collect::<Vec<_>>();
            let requirement =
                CompletionReviewRequirement::from_obligation_mode(&ledger.obligation.mode);
            let has_actionable_result = !findings.is_empty()
                || !manifest_gaps.is_empty()
                || dispositions.iter().any(|disposition| {
                    matches!(
                        disposition.disposition.as_str(),
                        "still_present" | "insufficient_proof" | "regressed"
                    )
                });
            let (disposition, attempted_outcome) = completion_review_attempt_dimensions(
                attempt_kind,
                &infrastructure_outcome,
                review_clean,
                has_actionable_result,
            );
            ledger.receipts.push(CompletionReviewReceiptV2 {
                review_id: review_id.clone(),
                attempt_kind,
                parent_review_id: parent_review_id.clone(),
                superseded_review_id,
                candidate_mutation_revision: dossier.host_mutation_revision,
                candidate_hash: dossier.implementation_identity_hash.clone(),
                implementation_identity_hash: dossier.implementation_identity_hash.clone(),
                dossier_snapshot_id: dossier.dossier_snapshot_id.clone(),
                user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                attempt_identity: attempt_identity.clone(),
                reviewer_contract_hash: reviewer_contract_hash.clone(),
                findings: findings.clone(),
                dispositions,
                manifest_gaps: manifest_gaps.clone(),
                repair_instruction_hash,
                repair_baseline: persisted_repair_baseline,
                baseline_hash: persisted_baseline_hash,
                input_mode: persisted_input_mode,
                delta_hash: persisted_delta_hash,
                rereview_delta: persisted_rereview_delta,
                fallback_reasons: persisted_fallback_reasons,
                candidate_implementation_identity: persisted_candidate_identity,
                rereview_audit_hash: persisted_rereview_audit_hash,
                requirement,
                disposition,
                attempted_outcome,
                infrastructure_outcome,
                review_clean,
                terminal_outcome: None,
                recorded_at: timestamp(),
            });
            let terminal_closure_id = terminal_outcome.as_deref().map(|outcome| {
                let terminal_review_id = format!(
                    "review-{}-{}-{}",
                    ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
                );
                ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
                ledger.receipts.push(CompletionReviewReceiptV2 {
                    review_id: terminal_review_id.clone(),
                    attempt_kind: CompletionReviewAttemptKind::TerminalClosure,
                    parent_review_id: Some(review_id.clone()),
                    superseded_review_id: None,
                    candidate_mutation_revision: dossier.host_mutation_revision,
                    candidate_hash: dossier.implementation_identity_hash.clone(),
                    implementation_identity_hash: dossier.implementation_identity_hash.clone(),
                    dossier_snapshot_id: dossier.dossier_snapshot_id.clone(),
                    user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                    requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                    attempt_identity: attempt_identity.clone(),
                    reviewer_contract_hash: reviewer_contract_hash.clone(),
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction_hash: None,
                    repair_baseline: None,
                    baseline_hash: None,
                    input_mode: None,
                    delta_hash: None,
                    rereview_delta: None,
                    fallback_reasons: Vec::new(),
                    candidate_implementation_identity: None,
                    rereview_audit_hash: None,
                    requirement,
                    disposition: CompletionReviewDisposition::NotApplicable,
                    attempted_outcome: None,
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: Some(outcome.to_string()),
                    recorded_at: timestamp(),
                });
                terminal_review_id
            });
            if reconstruct_manifest {
                let Some((_, _, requirements, mappings)) = gap_persistence.take() else {
                    unreachable!("manifest-gap reconstruction requires a prepared materialization");
                };
                let new_revision = ledger.manifest_revision.saturating_add(1);
                for (source_id, mapping) in mappings {
                    ledger.mapping_revisions.push(SourceMappingRevision {
                        completion_epoch: ledger.completion_epoch,
                        manifest_revision: new_revision,
                        source_id,
                        source_classification_contract_version: Some(
                            SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
                        ),
                        relationship_resolver_contract_version: Some(
                            RELATIONSHIP_RESOLVER_CONTRACT_VERSION.to_string(),
                        ),
                        mapping,
                    });
                }
                let manifest_hash = requirement_manifest_hash(new_revision, &requirements);
                let parent_terminal_review_id = ledger
                    .active_review_cycle
                    .as_ref()
                    .and_then(|cycle| cycle.parent_terminal_review_id.clone());
                let correction_consumed = ledger
                    .active_review_cycle
                    .as_ref()
                    .is_some_and(|cycle| cycle.correction_consumed);
                ledger.manifest_revision = new_revision;
                ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                    completion_epoch: ledger.completion_epoch,
                    manifest_revision: new_revision,
                    manifest_hash,
                    requirements,
                });
                ledger.active_review_cycle = Some(CompletionReviewCycle {
                    cycle_id: format!("cycle-{}-{new_revision}", ledger.completion_epoch),
                    manifest_revision: new_revision,
                    parent_terminal_review_id,
                    superseded_review_id: Some(review_id.clone()),
                    phase: CompletionReviewCyclePhase::InitialReviewPending,
                    correction_consumed,
                    manifest_gap_reconstructed: true,
                    accepted_review_id: None,
                    accepted_dossier_snapshot_id: None,
                });
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = ledger
                    .active_review_cycle
                    .as_ref()
                    .map(|cycle| cycle.cycle_id.clone());
                ledger.review_risk.opened_at = Some(timestamp());
                ledger.review_risk.resolved_at = None;
                // Reconstructing a review manifest is observational bookkeeping.  It does
                // not mutate the candidate or invalidate otherwise-current ordinary proof;
                // the fresh review cycle is what validates the corrected review surface.
            } else if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                if terminal_outcome.as_deref() == Some("partial") {
                    cycle.phase = CompletionReviewCyclePhase::TerminalPartial;
                } else if terminal_outcome.as_deref() == Some("blocked") {
                    cycle.phase = CompletionReviewCyclePhase::TerminalBlocked;
                } else {
                    match attempt_kind {
                        CompletionReviewAttemptKind::InitialReview => {
                            cycle.phase = if (needs_correction || !findings.is_empty())
                                && cycle.correction_consumed
                            {
                                CompletionReviewCyclePhase::TerminalPartial
                            } else if needs_correction {
                                CompletionReviewCyclePhase::CorrectionPending
                            } else if review_clean {
                                CompletionReviewCyclePhase::ProvisionalClean
                            } else if findings.is_empty() {
                                CompletionReviewCyclePhase::TerminalPartial
                            } else {
                                CompletionReviewCyclePhase::CorrectionPending
                            };
                        }
                        CompletionReviewAttemptKind::CorrectionEvidence => {
                            cycle.correction_consumed = true;
                            cycle.phase = CompletionReviewCyclePhase::RereviewPending;
                        }
                        CompletionReviewAttemptKind::Rereview => {
                            cycle.phase = if review_clean {
                                CompletionReviewCyclePhase::ProvisionalClean
                            } else {
                                CompletionReviewCyclePhase::TerminalPartial
                            };
                        }
                        CompletionReviewAttemptKind::TerminalClosure => unreachable!(),
                    }
                }
                if review_clean && !needs_correction && terminal_outcome.is_none() {
                    cycle.accepted_review_id = Some(review_id.clone());
                    cycle.accepted_dossier_snapshot_id = Some(dossier.dossier_snapshot_id.clone());
                    if ledger.obligation.mode == "mandatory"
                        && ledger.obligation.required_attempt_identity.as_deref()
                            == Some(attempt_identity.as_str())
                    {
                        ledger.obligation.satisfied_attempt_identity =
                            Some(attempt_identity.clone());
                    }
                }
                // Every persisted review attempt remains an unresolved review risk until the
                // terminal closure is committed atomically.  Classification can create the
                // cycle before `begin_completion_review_cycle` is called, so do not rely on
                // that transition alone to establish this invariant.
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = Some(cycle.cycle_id.clone());
                ledger.review_risk.resolved_at = None;
            }
            if let Some(terminal_closure_id) = terminal_closure_id {
                ledger.last_terminal_closure = Some(terminal_closure_id);
                let status = if terminal_outcome.as_deref() == Some("blocked") {
                    TaskCompletionStatus::Blocked
                } else {
                    TaskCompletionStatus::Partial
                };
                let mut reasons = dossier.evidence_gate.reasons.clone();
                if reasons.is_empty() {
                    reasons.push(format!(
                        "completion review ended {outcome}; see review {review_id}",
                        outcome = terminal_outcome.as_deref().unwrap_or("partial")
                    ));
                }
                document.completion = Some(TaskCompletionGate {
                    status,
                    reasons,
                    evidence_path,
                });
            } else {
                document.completion = None;
            }
            if let Some(current_validation_receipt_ids) = current_validation_receipt_ids
                && let Some((manifest_revision, source_hash, manifest_hash)) =
                    completion_contract_hashes(document, false)
            {
                for receipt in &mut document.command_receipts {
                    if current_validation_receipt_ids.contains(&receipt.id) {
                        receipt.manifest_revision = Some(manifest_revision);
                        receipt.user_source_ledger_hash = Some(source_hash.clone());
                        receipt.requirement_manifest_hash = Some(manifest_hash.clone());
                    }
                }
            }
            document.updated_at = timestamp();
            RecordedReviewAttempt {
                review_id,
                findings,
            }
        })
        .await
    }

    pub(crate) async fn finalize_completion_review(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<TaskCompletionGate> {
        if self.user_source_capture_failed()
            || !dossier.mappings_classified
            || dossier.sources.iter().any(|source| {
                source.availability != UserSourceAvailability::Available
                    || matches!(
                        dossier.source_mappings.get(&source.source_id),
                        Some(
                            SourceMapping::PendingClassification
                                | SourceMapping::UnavailableOrTruncated
                        )
                    )
            })
        {
            return AtomicReviewTransition::Failed;
        }
        let Some(accepted_review_id) = dossier.accepted_review_id.clone() else {
            return AtomicReviewTransition::Failed;
        };
        let (
            accepted_attempt_identity,
            accepted_reviewer_contract_hash,
            accepted_dossier_snapshot_id,
        ) = {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(cycle) = ledger.active_review_cycle.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(accepted) = ledger
                .receipts
                .iter()
                .find(|receipt| receipt.review_id == accepted_review_id)
            else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision
                || !ledger.review_risk.unresolved
                || cycle.phase != CompletionReviewCyclePhase::ProvisionalClean
                || cycle.accepted_review_id.as_deref() != Some(accepted_review_id.as_str())
                || cycle.accepted_dossier_snapshot_id.as_deref()
                    != Some(accepted.dossier_snapshot_id.as_str())
                || !accepted.review_clean
                || accepted.terminal_outcome.is_some()
                || accepted.implementation_identity_hash != dossier.implementation_identity_hash
                || accepted.user_source_ledger_hash != dossier.user_source_ledger_hash
                || accepted.requirement_manifest_hash != dossier.requirement_manifest_hash
            {
                return AtomicReviewTransition::Superseded;
            }
            (
                accepted.attempt_identity.clone(),
                accepted.reviewer_contract_hash.clone(),
                accepted.dossier_snapshot_id.clone(),
            )
        };

        let accepted_parent = accepted_review_id.clone();
        let completion = dossier.evidence_gate.clone();
        let terminal_outcome = completion_status_name(completion.status).to_string();
        let transition = self
            .atomic_review_update(
                dossier.document_revision,
                Some(&dossier.implementation_identity_hash),
                Some(&accepted_dossier_snapshot_id),
                |document| {
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        unreachable!("V2 dossier requires a V2 ledger");
                    };
                    let review_id = format!(
                        "review-{}-{}-{}",
                        ledger.completion_epoch,
                        ledger.manifest_revision,
                        ledger.next_review_sequence
                    );
                    ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
                    ledger.receipts.push(CompletionReviewReceiptV2 {
                        review_id: review_id.clone(),
                        attempt_kind: CompletionReviewAttemptKind::TerminalClosure,
                        parent_review_id: Some(accepted_parent),
                        superseded_review_id: None,
                        candidate_mutation_revision: dossier.host_mutation_revision,
                        candidate_hash: dossier.implementation_identity_hash.clone(),
                        implementation_identity_hash: dossier.implementation_identity_hash.clone(),
                        dossier_snapshot_id: accepted_dossier_snapshot_id.clone(),
                        user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                        requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                        attempt_identity: accepted_attempt_identity.clone(),
                        reviewer_contract_hash: accepted_reviewer_contract_hash.clone(),
                        findings: Vec::new(),
                        dispositions: Vec::new(),
                        manifest_gaps: Vec::new(),
                        repair_instruction_hash: None,
                        repair_baseline: None,
                        baseline_hash: None,
                        input_mode: None,
                        delta_hash: None,
                        rereview_delta: None,
                        fallback_reasons: Vec::new(),
                        candidate_implementation_identity: None,
                        rereview_audit_hash: None,
                        requirement: CompletionReviewRequirement::from_obligation_mode(
                            &ledger.obligation.mode,
                        ),
                        disposition: CompletionReviewDisposition::NotApplicable,
                        attempted_outcome: None,
                        infrastructure_outcome: "ok".to_string(),
                        review_clean: true,
                        terminal_outcome: Some(terminal_outcome.clone()),
                        recorded_at: timestamp(),
                    });
                    if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                        cycle.phase = CompletionReviewCyclePhase::Closed;
                    }
                    ledger.review_risk.unresolved = false;
                    ledger.review_risk.resolved_at = Some(timestamp());
                    ledger.last_terminal_closure = Some(review_id);
                    document.completion = Some(completion.clone());
                    document.updated_at = timestamp();
                    completion.clone()
                },
            )
            .await;
        if matches!(&transition, AtomicReviewTransition::Persisted(_)) {
            self.source_capture_failed.store(false, Ordering::Release);
        }
        transition
    }

    pub(crate) async fn passed_completion_matches_dossier(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> bool {
        if dossier.evidence_gate.status != TaskCompletionStatus::Passed
            || !dossier.typed_quiescent
            || !dossier.default_children_quiescent
            || !dossier.authoritative_input_errors.is_empty()
        {
            return false;
        }
        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return false;
        };
        let Some(ledger) = document.completion_review_v2.as_ref() else {
            return false;
        };
        let Some(cycle) = ledger.active_review_cycle.as_ref() else {
            return false;
        };
        let Some(accepted_review_id) = cycle.accepted_review_id.as_deref() else {
            return false;
        };
        let Some(terminal_id) = ledger.last_terminal_closure.as_deref() else {
            return false;
        };
        let Some(terminal) = ledger
            .receipts
            .iter()
            .find(|receipt| receipt.review_id == terminal_id)
        else {
            return false;
        };
        let Some(accepted) = ledger
            .receipts
            .iter()
            .find(|receipt| receipt.review_id == accepted_review_id)
        else {
            return false;
        };
        document.revision == dossier.document_revision
            && document
                .completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
            && cycle.phase == CompletionReviewCyclePhase::Closed
            && !ledger.review_risk.unresolved
            && cycle.accepted_dossier_snapshot_id.as_deref()
                == Some(accepted.dossier_snapshot_id.as_str())
            && accepted.review_clean
            && accepted.implementation_identity_hash == dossier.implementation_identity_hash
            && accepted.user_source_ledger_hash == dossier.user_source_ledger_hash
            && accepted.requirement_manifest_hash == dossier.requirement_manifest_hash
            && terminal.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
            && terminal.terminal_outcome.as_deref() == Some("passed")
            && terminal.parent_review_id.as_deref() == Some(accepted_review_id)
            && receipt_identity_matches(terminal, accepted)
            && terminal.implementation_identity_hash == dossier.implementation_identity_hash
            && terminal.user_source_ledger_hash == dossier.user_source_ledger_hash
            && terminal.requirement_manifest_hash == dossier.requirement_manifest_hash
    }

    pub(crate) async fn supersede_provisional_completion_review(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<()> {
        self.atomic_review_update(dossier.document_revision, None, None, |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            if let Some(cycle) = ledger.active_review_cycle.as_mut()
                && cycle.phase == CompletionReviewCyclePhase::ProvisionalClean
            {
                cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = Some(cycle.cycle_id.clone());
                ledger.review_risk.opened_at = Some(timestamp());
                ledger.review_risk.resolved_at = None;
            }
            document.completion = None;
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn completion_review_correction_consumed(&self) -> bool {
        self.document
            .lock()
            .await
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .and_then(|ledger| ledger.active_review_cycle.as_ref())
            .is_some_and(|cycle| cycle.correction_consumed)
    }

    pub(crate) async fn prepare_after_agent_completion_review_reentry(
        &self,
        preserve_correction_consumed: bool,
    ) -> AtomicReviewTransition<()> {
        let Some(expected_revision) = self.document_revision().await else {
            return AtomicReviewTransition::Failed;
        };
        self.atomic_review_update(expected_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            let parent_terminal_review_id = ledger
                .receipts
                .iter()
                .rev()
                .find(|receipt| {
                    receipt.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
                })
                .map(|receipt| receipt.review_id.clone());
            let prior_consumed = ledger
                .active_review_cycle
                .as_ref()
                .is_some_and(|cycle| cycle.correction_consumed);
            let correction_consumed = prior_consumed || preserve_correction_consumed;
            let manifest_gap_reconstructed = ledger
                .active_review_cycle
                .as_ref()
                .is_some_and(|cycle| cycle.manifest_gap_reconstructed);
            let superseded_review_id = ledger
                .active_review_cycle
                .as_ref()
                .and_then(|cycle| cycle.superseded_review_id.clone());
            let cycle_id = ledger
                .active_review_cycle
                .as_ref()
                .map(|cycle| cycle.cycle_id.clone())
                .unwrap_or_else(|| {
                    format!(
                        "cycle-{}-{}-after-agent",
                        ledger.completion_epoch, ledger.manifest_revision
                    )
                });
            ledger.active_review_cycle = Some(CompletionReviewCycle {
                cycle_id: cycle_id.clone(),
                manifest_revision: ledger.manifest_revision,
                parent_terminal_review_id,
                superseded_review_id,
                phase: CompletionReviewCyclePhase::InitialReviewPending,
                correction_consumed,
                manifest_gap_reconstructed,
                accepted_review_id: None,
                accepted_dossier_snapshot_id: None,
            });
            ledger.review_risk.unresolved = true;
            ledger.review_risk.cycle_id = Some(cycle_id);
            ledger.review_risk.opened_at = Some(timestamp());
            ledger.review_risk.resolved_at = None;
            document.completion = None;
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn invalidate_completion_after_terminal_emission_failure(
        &self,
        reason: &str,
    ) -> AtomicReviewTransition<()> {
        let Some(expected_revision) = self.document_revision().await else {
            return AtomicReviewTransition::Failed;
        };
        let reason = reason.to_string();
        self.atomic_review_update(expected_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            let persisted_passed = document
                .completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
            if !persisted_passed {
                return;
            }
            let Some(superseded_review_id) = ledger.last_terminal_closure.clone() else {
                return;
            };
            let Some(superseded) = ledger
                .receipts
                .iter()
                .find(|receipt| receipt.review_id == superseded_review_id)
                .cloned()
            else {
                return;
            };
            let review_id = format!(
                "review-{}-{}-{}",
                ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
            );
            ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
            ledger.receipts.push(CompletionReviewReceiptV2 {
                review_id: review_id.clone(),
                attempt_kind: CompletionReviewAttemptKind::TerminalClosure,
                parent_review_id: superseded.parent_review_id.clone(),
                superseded_review_id: Some(superseded_review_id),
                candidate_mutation_revision: superseded.candidate_mutation_revision,
                candidate_hash: superseded.candidate_hash.clone(),
                implementation_identity_hash: superseded.implementation_identity_hash.clone(),
                dossier_snapshot_id: superseded.dossier_snapshot_id.clone(),
                user_source_ledger_hash: superseded.user_source_ledger_hash.clone(),
                requirement_manifest_hash: superseded.requirement_manifest_hash,
                attempt_identity: superseded.attempt_identity.clone(),
                reviewer_contract_hash: superseded.reviewer_contract_hash.clone(),
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction_hash: None,
                repair_baseline: None,
                baseline_hash: None,
                input_mode: None,
                delta_hash: None,
                rereview_delta: None,
                fallback_reasons: Vec::new(),
                candidate_implementation_identity: None,
                rereview_audit_hash: None,
                requirement: superseded.requirement,
                disposition: CompletionReviewDisposition::NotApplicable,
                attempted_outcome: None,
                infrastructure_outcome: format!("terminal_emission_failure:{reason}"),
                review_clean: false,
                terminal_outcome: Some("partial".to_string()),
                recorded_at: timestamp(),
            });
            if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                cycle.phase = CompletionReviewCyclePhase::TerminalPartial;
            }
            ledger.review_risk.unresolved = true;
            ledger.review_risk.resolved_at = None;
            ledger.last_terminal_closure = Some(review_id);
            let mut completion = document.completion.clone().unwrap_or(TaskCompletionGate {
                status: TaskCompletionStatus::Partial,
                reasons: Vec::new(),
                evidence_path: None,
            });
            completion.status = TaskCompletionStatus::Partial;
            completion.reasons.push(reason);
            completion.reasons.sort();
            completion.reasons.dedup();
            document.completion = Some(completion);
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn apply_source_classification(
        &self,
        dossier: &CompletionReviewDossier,
        materialization: SourceMaterialization,
    ) -> AtomicReviewTransition<()> {
        let Some(prepared) = prepare_source_materialization(dossier, materialization) else {
            return AtomicReviewTransition::Failed;
        };
        let PreparedSourceMaterialization {
            local_classifications,
            requirements,
            mappings,
        } = prepared;
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision {
                return AtomicReviewTransition::Superseded;
            }
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if !dossier_sources_are_current(ledger, &dossier.sources) {
                return AtomicReviewTransition::Failed;
            }
        }
        let replacement_cache_keys = local_classifications
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let replacement_cache_entries = local_classifications
            .into_iter()
            .map(|(key, classification)| SourceClassificationCacheEntry {
                contract_version: key.contract_version,
                source_kind: key.source_kind,
                content_hash: key.content_hash,
                classification,
            })
            .collect::<Vec<_>>();
        let classified_manifest_revision = dossier.manifest_revision.saturating_add(1);
        let manifest_hash = requirement_manifest_hash(classified_manifest_revision, &requirements);
        self.atomic_review_update(dossier.document_revision, None, None, move |document| {
            let current_validation_receipt_ids = document
                .command_receipts
                .iter()
                .filter(|receipt| command_receipt_has_current_proof_identity(document, receipt))
                .map(|receipt| receipt.id.clone())
                .collect::<BTreeSet<_>>();
            document
                .source_classification_cache
                .retain(|entry| !replacement_cache_keys.contains(&entry.key()));
            document
                .source_classification_cache
                .extend(replacement_cache_entries);
            {
                let Some(ledger) = document.completion_review_v2.as_mut() else {
                    return;
                };
                ledger.mapping_revisions.extend(mappings.into_iter().map(
                    |(source_id, mapping)| SourceMappingRevision {
                        completion_epoch: ledger.completion_epoch,
                        manifest_revision: classified_manifest_revision,
                        source_id,
                        source_classification_contract_version: Some(
                            SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
                        ),
                        relationship_resolver_contract_version: Some(
                            RELATIONSHIP_RESOLVER_CONTRACT_VERSION.to_string(),
                        ),
                        mapping,
                    },
                ));
                ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                    completion_epoch: ledger.completion_epoch,
                    manifest_revision: classified_manifest_revision,
                    manifest_hash,
                    requirements,
                });
                ledger.manifest_revision = classified_manifest_revision;
                if let Some(cycle) = ledger.active_review_cycle.as_mut()
                    && cycle.phase == CompletionReviewCyclePhase::ClassificationPending
                {
                    cycle.manifest_revision = classified_manifest_revision;
                    cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                }
            }
            if let Some((manifest_revision, source_hash, manifest_hash)) =
                completion_contract_hashes(document, false)
            {
                for receipt in &mut document.command_receipts {
                    if current_validation_receipt_ids.contains(&receipt.id) {
                        receipt.manifest_revision = Some(manifest_revision);
                        receipt.user_source_ledger_hash = Some(source_hash.clone());
                        receipt.requirement_manifest_hash = Some(manifest_hash.clone());
                    }
                }
            }
            // Source classification is observational review materialization. It
            // must not invalidate ordinary validation or completion proof.
            document.updated_at = timestamp();
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn completion_review_evidence_summary(
        &self,
        gate: &TaskCompletionGate,
    ) -> String {
        let mut lines = vec![format!(
            "Evidence gate: {}",
            completion_status_name(gate.status)
        )];
        lines.extend(
            gate.reasons
                .iter()
                .map(|reason| format!("Gate reason: {reason}")),
        );

        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return lines.join("\n");
        };
        for step in &document.plan {
            lines.push(format!(
                "Plan step {} [{:?}]: {}",
                step.id, step.status, step.step
            ));
            lines.extend(
                step.acceptance_criteria
                    .iter()
                    .map(|criterion| format!("Acceptance criterion: {criterion}")),
            );
        }
        for snapshot in document.latest_file_hashes.values() {
            lines.push(format!(
                "Changed path: {} (exists: {}, sha1: {})",
                snapshot.path,
                snapshot.exists,
                snapshot.sha1.as_deref().unwrap_or("unavailable")
            ));
        }
        let mut prior_epoch_receipt_count = 0usize;
        for receipt in &document.command_receipts {
            if receipt.epoch != document.evidence_epoch {
                prior_epoch_receipt_count = prior_epoch_receipt_count.saturating_add(1);
                continue;
            }
            let outcome = if receipt.timed_out {
                "timed_out"
            } else if receipt.exit_code == 0 {
                "succeeded"
            } else {
                "failed"
            };
            lines.push(format!(
                "Command receipt [current epoch {}, outcome: {}, possible mutation: {}]: {}",
                receipt.epoch,
                outcome,
                receipt.possible_mutation,
                receipt.command.join(" "),
            ));
        }
        if prior_epoch_receipt_count > 0 {
            lines.push(format!(
                "Prior-epoch command receipts omitted: {prior_epoch_receipt_count}"
            ));
        }
        lines.join("\n")
    }

    #[cfg(test)]
    pub(crate) async fn record_completion_review_audit(
        &self,
        turn_id: &str,
        outcome: &str,
        failure_category: Option<&str>,
        finding_summary: Vec<String>,
        repair_injected: bool,
    ) -> bool {
        self.record_completion_review_audit_with_measurements(
            turn_id,
            outcome,
            failure_category,
            finding_summary,
            repair_injected,
            CompletionReviewAuditMeasurements::default(),
        )
        .await
    }

    pub(crate) async fn record_completion_review_audit_with_measurements(
        &self,
        turn_id: &str,
        outcome: &str,
        failure_category: Option<&str>,
        finding_summary: Vec<String>,
        repair_injected: bool,
        measurements: CompletionReviewAuditMeasurements,
    ) -> bool {
        if !self.allows_kd4_completion() {
            return false;
        }
        let Some(((), snapshot)) = self
            .update_document(|document| {
                document
                    .completion_review_receipts
                    .push(CompletionReviewAuditReceipt {
                        turn_id: turn_id.to_string(),
                        recorded_at: timestamp(),
                        evidence_epoch: document.evidence_epoch,
                        outcome: outcome.to_string(),
                        failure_category: failure_category.map(str::to_string),
                        finding_summary,
                        repair_injected,
                        measurements,
                    });
                trim_to_last(
                    &mut document.completion_review_receipts,
                    MAX_COMPLETION_REVIEW_RECEIPTS,
                );
                document.updated_at = timestamp();
            })
            .await
        else {
            return false;
        };
        if self.persist_document(&snapshot).await != PersistOutcome::Persisted {
            warn!("failed to persist observational completion-review audit receipt");
            return false;
        }
        true
    }

    pub(crate) async fn synchronize_completion_review_obligation(
        &self,
        input: CompletionReviewObligationInput,
    ) -> AtomicReviewTransition<()> {
        if !matches!(
            input.mode.as_str(),
            "mandatory" | "supplemental" | "disabled"
        ) || (input.mode == "mandatory"
            && (input.requirement_ids.is_empty() || input.obligation_hash.is_empty()))
        {
            return AtomicReviewTransition::Failed;
        }
        let Some(expected_revision) = self.document_revision().await else {
            return AtomicReviewTransition::Failed;
        };
        self.atomic_review_update(expected_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            let changed = ledger.obligation.mode != input.mode
                || ledger.obligation.requirement_ids != input.requirement_ids
                || ledger.obligation.obligation_hash != input.obligation_hash;
            if changed {
                ledger.obligation = CompletionReviewObligationState {
                    mode: input.mode.clone(),
                    requirement_ids: input.requirement_ids.clone(),
                    obligation_hash: input.obligation_hash.clone(),
                    required_attempt_identity: input.required_attempt_identity.clone(),
                    satisfied_attempt_identity: None,
                };
                if input.mode == "mandatory" {
                    document.completion = None;
                }
            } else if let Some(identity) = input.required_attempt_identity
                && ledger.obligation.required_attempt_identity.as_deref() != Some(identity.as_str())
            {
                ledger.obligation.required_attempt_identity = Some(identity);
                ledger.obligation.satisfied_attempt_identity = None;
                if ledger.obligation.mode == "mandatory" {
                    document.completion = None;
                }
            }
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn prior_completion_review_attempt(
        &self,
        attempt_identity: &str,
    ) -> Option<PriorCompletionReviewAttempt> {
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        if let Some(receipt) = document
            .completion_review_v2
            .as_ref()?
            .receipts
            .iter()
            .rev()
            .find(|receipt| {
                receipt.attempt_identity == attempt_identity
                    && receipt.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
            })
        {
            if receipt.review_clean && receipt.infrastructure_outcome == "ok" {
                return Some(PriorCompletionReviewAttempt::Clean);
            }
            if !receipt.findings.is_empty() && receipt.infrastructure_outcome == "ok" {
                return Some(PriorCompletionReviewAttempt::Actionable);
            }
        }
        document
            .completion_review_receipts
            .iter()
            .rev()
            .find(|receipt| {
                receipt.measurements.attempt_identity == attempt_identity
                    && receipt.measurements.failure_class == "deterministic"
            })
            .map(|_| PriorCompletionReviewAttempt::DeterministicInfrastructure)
    }

    pub(crate) async fn reviewer_infrastructure_memo_matches(
        &self,
        candidate_id: &str,
        dossier_id: &str,
        reviewer_configuration_identity: &str,
        infrastructure_condition_identity: &str,
    ) -> bool {
        let identity = reviewer_infrastructure_memo_identity(
            candidate_id,
            dossier_id,
            reviewer_configuration_identity,
            infrastructure_condition_identity,
        );
        self.document
            .lock()
            .await
            .as_ref()
            .and_then(|document| document.final_proof.reviewer_infrastructure_memo.as_ref())
            .is_some_and(|memo| memo.identity == identity)
    }

    pub(crate) async fn record_reviewer_infrastructure_memo(
        &self,
        candidate_id: String,
        dossier_id: String,
        reviewer_configuration_identity: String,
        infrastructure_condition_identity: String,
        outcome: String,
    ) -> bool {
        let identity = reviewer_infrastructure_memo_identity(
            &candidate_id,
            &dossier_id,
            &reviewer_configuration_identity,
            &infrastructure_condition_identity,
        );
        let Some(((), snapshot)) = self
            .update_document(move |document| {
                document.final_proof.reviewer_infrastructure_memo =
                    Some(ReviewerInfrastructureMemoV1 {
                        identity,
                        candidate_id,
                        dossier_id,
                        reviewer_configuration_identity,
                        infrastructure_condition_identity,
                        outcome,
                    });
                document.updated_at = timestamp();
            })
            .await
        else {
            return false;
        };
        self.persist_document(&snapshot).await == PersistOutcome::Persisted
    }

    pub(crate) async fn reuse_completion_review_clean_proof(
        &self,
        attempt_identity: &str,
    ) -> AtomicReviewTransition<()> {
        let Some(expected_revision) = self.document_revision().await else {
            return AtomicReviewTransition::Failed;
        };
        let attempt_identity = attempt_identity.to_string();
        self.atomic_review_update(expected_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            let reusable = ledger.receipts.iter().any(|receipt| {
                receipt.attempt_identity == attempt_identity
                    && receipt.review_clean
                    && receipt.infrastructure_outcome == "ok"
            });
            if reusable
                && ledger.obligation.mode == "mandatory"
                && ledger.obligation.required_attempt_identity.as_deref()
                    == Some(attempt_identity.as_str())
            {
                ledger.obligation.satisfied_attempt_identity = Some(attempt_identity);
            }
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn abandon_completion_review_cycle(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<()> {
        self.atomic_review_update(dossier.document_revision, None, None, |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                cycle.phase = CompletionReviewCyclePhase::Closed;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
            }
            ledger.review_risk.unresolved = false;
            ledger.review_risk.resolved_at = Some(timestamp());
            document.updated_at = timestamp();
        })
        .await
    }

    /// Returns the correctness identities owned by task evidence for the final
    /// proof boundary. The caller adds host-owned workspace, environment, diff,
    /// and reviewer identities only after terminal quiescence is established.
    pub(crate) async fn final_proof_identity_snapshot(
        &self,
    ) -> Option<FinalProofIdentitySnapshotV1> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        let completion_review = document.completion_review_v2.as_ref();
        let source_identity = completion_review
            .map(|ledger| {
                user_source_ledger_snapshot_hash(
                    ledger,
                    ledger.completion_epoch,
                    ledger.manifest_revision,
                    ledger.source_capture_failed,
                )
            })
            .unwrap_or_else(|| canonical_hash("KD4_EMPTY_USER_SOURCE_LEDGER_V1", &Value::Null));
        let requirement_identity = completion_review
            .and_then(active_manifest)
            .map(|manifest| manifest.manifest_hash.clone())
            .unwrap_or_else(|| canonical_hash("KD4_EMPTY_REQUIREMENT_MANIFEST_V1", &Value::Null));
        let implementation_identity = document
            .command_receipts
            .iter()
            .rev()
            .find(|receipt| {
                receipt.epoch == document.evidence_epoch
                    && receipt.host_mutation_revision == Some(document.host_mutation_revision)
                    && receipt.user_source_ledger_hash.as_deref() == Some(source_identity.as_str())
                    && receipt.requirement_manifest_hash.as_deref()
                        == Some(requirement_identity.as_str())
            })
            .and_then(|receipt| receipt.implementation_identity_hash.clone())
            .unwrap_or_else(|| {
                canonical_hash(
                    "KD4_FINAL_PROOF_IMPLEMENTATION_IDENTITY_V1",
                    &serde_json::json!({
                        "evidence_epoch": document.evidence_epoch,
                        "host_mutation_revision": document.host_mutation_revision,
                        "plan": document.plan,
                        "work_unit": document.planning.work_unit,
                        "latest_file_hashes": document.latest_file_hashes,
                    }),
                )
            });
        let mut changed_paths = document
            .latest_file_hashes
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        changed_paths.sort();
        changed_paths.dedup();
        let workspace_path_snapshot_identity = canonical_hash(
            "KD4_FINAL_PROOF_WORKSPACE_PATH_SNAPSHOTS_V1",
            &serde_json::to_value(&document.latest_file_hashes).unwrap_or(Value::Null),
        );
        Some(FinalProofIdentitySnapshotV1 {
            implementation_identity,
            source_identity,
            requirement_identity,
            task_evidence_epoch: document.evidence_epoch,
            host_mutation_revision: document.host_mutation_revision,
            changed_paths,
            workspace_path_snapshot_identity,
        })
    }

    /// Seals the current correctness-affecting completion basis and its deterministic
    /// validation plan. Callers refresh hook and child-lifecycle inputs first, but writers
    /// remain unfrozen; later workspace observations invalidate or refresh this basis.
    pub(crate) async fn seal_final_proof_candidate(
        &self,
        input: FinalProofSealInputV1,
    ) -> Option<FinalProofSealResultV1> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let evidence_path = self
            .evidence_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let desktop_activation_runtime = self.desktop_activation_runtime_snapshot();
        let ((result, changed), snapshot) = self
            .update_document(move |document| {
                let basis = completion_candidate_basis(document, &input);
                let validation_plan = validation_plan_for_basis(document, &basis);
                let candidate = completion_candidate_for(&basis, &validation_plan);
                let mut diff_snapshot = input.diff_snapshot.clone();
                diff_snapshot
                    .candidate_id
                    .clone_from(&candidate.candidate_id);
                diff_snapshot.bounded_hunks = truncate_checkpoint_hunks(
                    &diff_snapshot.bounded_hunks,
                    MAX_COMPLETION_CHECKPOINT_HUNK_BYTES,
                );

                let validation_aggregate_started = Instant::now();
                let (proof_observations, validation_launch_count, validation_process_ns) =
                    current_final_proof_observations(
                        document,
                        &basis,
                        &candidate,
                        &validation_plan,
                    );
                let telemetry = FinalProofSealTelemetryV1 {
                    validation_launch_count,
                    validation_process_ns,
                    validation_aggregate_ns: u64::try_from(
                        validation_aggregate_started.elapsed().as_nanos(),
                    )
                    .unwrap_or(u64::MAX),
                };
                let mut gate = derive_completion_gate(
                    document,
                    self.evidence_path.as_deref(),
                    &desktop_activation_runtime,
                );
                let missing_or_failed = missing_or_failed_obligations(
                    &candidate,
                    &validation_plan,
                    &proof_observations,
                    document.evidence_epoch,
                );
                let mut structured_reasons = document.final_proof.reasons.clone();
                for obligation_id in &missing_or_failed {
                    let occurrence = format!(
                        "final proof obligation {obligation_id} is missing, failed, or stale"
                    );
                    gate.reasons.push(occurrence.clone());
                    merge_completion_reason(
                        &mut structured_reasons,
                        "missing_failed_or_stale_proof",
                        std::slice::from_ref(obligation_id),
                        &[],
                        &[],
                        document.evidence_epoch,
                        &occurrence,
                        None,
                    );
                }
                if validation_plan.ambiguous_or_unmappable {
                    let occurrence =
                        "the sealed validation plan is ambiguous or unmappable".to_string();
                    gate.reasons.push(occurrence.clone());
                    merge_completion_reason(
                        &mut structured_reasons,
                        "invalid_validation_plan",
                        &[],
                        &[],
                        &[],
                        document.evidence_epoch,
                        &occurrence,
                        None,
                    );
                }
                if !basis.child_gate_state.is_empty() {
                    for state in &basis.child_gate_state {
                        let occurrence = format!("child or typed gate is not clear: {state}");
                        gate.reasons.push(occurrence.clone());
                        merge_completion_reason(
                            &mut structured_reasons,
                            "child_or_typed_gate_blocked",
                            &[],
                            &[],
                            &[],
                            document.evidence_epoch,
                            &occurrence,
                            None,
                        );
                    }
                }
                gate.reasons.sort();
                gate.reasons.dedup();
                if !gate.reasons.is_empty() && gate.status == TaskCompletionStatus::Passed {
                    gate.status = TaskCompletionStatus::Partial;
                }

                let failure_fingerprint = completion_failure_fingerprint(
                    document.evidence_epoch,
                    &candidate,
                    &missing_or_failed,
                    &basis.child_gate_state,
                    None,
                );
                if document.final_proof.candidate.as_ref() == Some(&candidate)
                    && document.final_proof.failure_fingerprint.as_ref()
                        == Some(&failure_fingerprint)
                    && let Some(decision) = document.final_proof.terminal_decision.clone()
                {
                    return (
                        FinalProofSealResultV1::Memoized {
                            gate: decision,
                            checkpoint_tokens: document
                                .final_proof
                                .checkpoint
                                .as_ref()
                                .map(|checkpoint| checkpoint.estimated_tokens)
                                .unwrap_or_default(),
                            telemetry,
                        },
                        false,
                    );
                }

                let checkpoint = match completion_checkpoint_for(
                    document,
                    &basis,
                    &candidate,
                    &validation_plan,
                    &diff_snapshot,
                    &proof_observations,
                    input.checkpoint_token_budget,
                ) {
                    Ok(checkpoint) => checkpoint,
                    Err(reason) => {
                        gate.status = TaskCompletionStatus::Partial;
                        gate.reasons.push(reason.clone());
                        gate.reasons.sort();
                        gate.reasons.dedup();
                        merge_completion_reason(
                            &mut structured_reasons,
                            "checkpoint_preflight_failed",
                            &[],
                            &[],
                            &[],
                            document.evidence_epoch,
                            &reason,
                            None,
                        );
                        document.final_proof = FinalProofStateV1 {
                            basis: Some(basis),
                            validation_plan: Some(validation_plan),
                            candidate: Some(candidate),
                            diff_snapshot: Some(diff_snapshot),
                            proof_observations,
                            failure_fingerprint: Some(failure_fingerprint),
                            terminal_decision: Some(gate.clone()),
                            reasons: structured_reasons,
                            reviewer_infrastructure_memo: document
                                .final_proof
                                .reviewer_infrastructure_memo
                                .clone(),
                            repair_count_by_lineage: document
                                .final_proof
                                .repair_count_by_lineage
                                .clone(),
                            ..FinalProofStateV1::default()
                        };
                        document.completion = Some(gate.clone());
                        document.updated_at = timestamp();
                        return (FinalProofSealResultV1::PreflightFailed(gate), true);
                    }
                };

                document.final_proof = FinalProofStateV1 {
                    basis: Some(basis),
                    validation_plan: Some(validation_plan.clone()),
                    candidate: Some(candidate.clone()),
                    diff_snapshot: Some(diff_snapshot),
                    proof_observations,
                    failure_fingerprint: Some(failure_fingerprint),
                    terminal_decision: Some(gate.clone()),
                    reasons: structured_reasons,
                    checkpoint: Some(checkpoint.clone()),
                    reviewer_infrastructure_memo: document
                        .final_proof
                        .reviewer_infrastructure_memo
                        .clone(),
                    finalization_memo: document.final_proof.finalization_memo.clone().filter(
                        |memo| {
                            memo.candidate_id == candidate.candidate_id
                                && memo.checkpoint_id == checkpoint.checkpoint_id
                        },
                    ),
                    repair_count_by_lineage: document.final_proof.repair_count_by_lineage.clone(),
                };
                document.updated_at = timestamp();
                (
                    FinalProofSealResultV1::Sealed {
                        candidate,
                        validation_plan,
                        checkpoint: Box::new(checkpoint),
                        telemetry,
                        gate,
                    },
                    true,
                )
            })
            .await?;
        if changed {
            match self.persist_document(&snapshot).await {
                PersistOutcome::Persisted => {}
                PersistOutcome::Superseded | PersistOutcome::Failed => {
                    return Some(FinalProofSealResultV1::PreflightFailed(
                        TaskCompletionGate {
                            status: TaskCompletionStatus::Partial,
                            reasons: vec![
                                "the sealed final-proof state could not be durably persisted"
                                    .to_string(),
                            ],
                            evidence_path,
                        },
                    ));
                }
            }
        }
        Some(result)
    }

    pub(crate) async fn completion_checkpoint_payload(&self) -> Option<(String, String)> {
        let guard = self.document.lock().await;
        let checkpoint = guard.as_ref()?.final_proof.checkpoint.as_ref()?;
        Some((
            checkpoint.checkpoint_id.clone(),
            checkpoint.canonical_payload()?,
        ))
    }

    pub(crate) async fn memoized_finalization_result(&self, turn_id: &str) -> Option<String> {
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        let candidate = document.final_proof.candidate.as_ref()?;
        let checkpoint = document.final_proof.checkpoint.as_ref()?;
        let memo = document.final_proof.finalization_memo.as_ref()?;
        let recovery_identity = completion_recovery_identity(
            document.final_proof.basis.as_ref()?,
            turn_id,
            memo.terminal_hooks_completed,
            memo.mutation_quiescent,
        );
        (memo.turn_id.as_deref() == Some(turn_id)
            && memo.terminal_hooks_completed
            && memo.mutation_quiescent
            && memo.recovery_identity.as_ref() == Some(&recovery_identity)
            && memo.candidate_id == candidate.candidate_id
            && memo.checkpoint_id == checkpoint.checkpoint_id)
            .then(|| memo.final_message.clone())
    }

    pub(crate) async fn completion_recovery_intent(
        &self,
        open_turn_id: &str,
    ) -> Option<CompletionRecoveryIntentV1> {
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        let candidate = document.final_proof.candidate.as_ref()?;
        let checkpoint = document.final_proof.checkpoint.as_ref()?;
        let memo = document.final_proof.finalization_memo.as_ref()?;
        let recovery_identity = completion_recovery_identity(
            document.final_proof.basis.as_ref()?,
            open_turn_id,
            memo.terminal_hooks_completed,
            memo.mutation_quiescent,
        );
        (memo.turn_id.as_deref() == Some(open_turn_id)
            && memo.terminal_hooks_completed
            && memo.mutation_quiescent
            && memo.recovery_identity.as_ref() == Some(&recovery_identity)
            && memo.candidate_id == candidate.candidate_id
            && memo.checkpoint_id == checkpoint.checkpoint_id)
            .then(|| CompletionRecoveryIntentV1 {
                turn_id: open_turn_id.to_string(),
                memo_identity: memo.identity.clone(),
                final_message: memo.final_message.clone(),
            })
    }

    pub(crate) async fn current_finalization_memo_identity(&self) -> Option<String> {
        self.document
            .lock()
            .await
            .as_ref()?
            .final_proof
            .finalization_memo
            .as_ref()
            .map(|memo| memo.identity.clone())
    }

    pub(crate) async fn record_finalization_result(
        &self,
        turn_id: String,
        final_message: String,
        terminal_hooks_completed: bool,
        mutation_quiescent: bool,
    ) -> bool {
        let Some((recorded, snapshot)) = self
            .update_document(|document| {
                let (Some(basis), Some(candidate), Some(checkpoint)) = (
                    document.final_proof.basis.as_ref(),
                    document.final_proof.candidate.as_ref(),
                    document.final_proof.checkpoint.as_ref(),
                ) else {
                    return false;
                };
                let recovery_identity = completion_recovery_identity(
                    basis,
                    &turn_id,
                    terminal_hooks_completed,
                    mutation_quiescent,
                );
                let identity = canonical_hash(
                    "KD4_COMPLETION_FINALIZATION_MEMO_V1",
                    &serde_json::json!({
                        "candidate_id": candidate.candidate_id,
                        "checkpoint_id": checkpoint.checkpoint_id,
                        "turn_id": &turn_id,
                        "terminal_hooks_completed": terminal_hooks_completed,
                        "mutation_quiescent": mutation_quiescent,
                        "recovery_identity": &recovery_identity,
                        "final_message": &final_message,
                    }),
                );
                document.final_proof.finalization_memo = Some(CompletionFinalizationMemoV1 {
                    identity,
                    candidate_id: candidate.candidate_id.clone(),
                    checkpoint_id: checkpoint.checkpoint_id.clone(),
                    turn_id: Some(turn_id),
                    terminal_hooks_completed,
                    mutation_quiescent,
                    recovery_identity: Some(recovery_identity),
                    final_message,
                });
                document.updated_at = timestamp();
                true
            })
            .await
        else {
            return false;
        };
        recorded && self.persist_document(&snapshot).await == PersistOutcome::Persisted
    }

    pub(crate) async fn completion_gate(&self) -> Option<TaskCompletionGate> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let source_capture_failed = self.user_source_capture_failed();
        let mut latest_gate = None;
        for persistence_retry in 0..8 {
            if persistence_retry > 0 {
                let mut state = self.lock_freshness_state();
                state.diagnostics.conservative_reruns =
                    state.diagnostics.conservative_reruns.saturating_add(1);
            }
            let freshness_ready = self
                .refresh_external_file_freshness_for(if persistence_retry == 0 {
                    FreshnessPurpose::CompletionFresh
                } else {
                    FreshnessPurpose::CompletionRetry
                })
                .await;
            if !freshness_ready {
                continue;
            }
            let completion_proof_manifest = self.completion_proof_manifest();
            let desktop_activation_runtime = self.desktop_activation_runtime_snapshot();
            let mut freshness_state_changed = false;
            let (gate, snapshot) = self
                .update_document(|document| {
                    if !task_is_tracked(document) {
                        return None;
                    }
                    let current_manifest = freshness_manifest(document);
                    if (!current_manifest.tracked.is_empty()
                        || !current_manifest.artifact_paths.is_empty())
                        && completion_proof_manifest.as_ref() != Some(&current_manifest)
                    {
                        freshness_state_changed = true;
                        return None;
                    }
                    if self.evidence_path.is_some() {
                        resolve_risk(document, "task-evidence-storage-failure");
                    }
                    let mut gate = derive_completion_gate(
                        document,
                        self.evidence_path.as_deref(),
                        &desktop_activation_runtime,
                    );
                    overlay_completion_review_gate(document, &mut gate, source_capture_failed);
                    document.completion = Some(gate.clone());
                    document.updated_at = timestamp();
                    Some(gate)
                })
                .await?;
            if freshness_state_changed {
                continue;
            }
            let gate = gate?;
            latest_gate = Some(gate.clone());
            match self.persist_document(&snapshot).await {
                PersistOutcome::Persisted => return Some(gate),
                PersistOutcome::Superseded => continue,
                PersistOutcome::Failed => {
                    return Some(
                        self.block_gate_for_persistence(
                            gate,
                            Some(snapshot.revision),
                            "task-evidence persistence failed; completion is not durably recorded",
                            true,
                        )
                        .await,
                    );
                }
            }
        }
        let gate = latest_gate?;
        Some(
            self.block_gate_for_persistence(
                gate,
                None,
                "task-evidence changed repeatedly while completion was being persisted; a stable completion snapshot was not recorded",
                false,
            )
            .await,
        )
    }

    async fn block_gate_for_persistence(
        &self,
        mut gate: TaskCompletionGate,
        snapshot_revision: Option<u64>,
        reason: &str,
        record_storage_risk: bool,
    ) -> TaskCompletionGate {
        gate.reasons.push(reason.to_string());
        gate.reasons.sort();
        gate.reasons.dedup();
        if gate.status != TaskCompletionStatus::Blocked {
            gate.status = TaskCompletionStatus::Partial;
        }
        let desktop_activation_runtime = self.desktop_activation_runtime_snapshot();
        let snapshot = {
            let mut guard = self.document.lock().await;
            guard.as_mut().map(|document| {
                if record_storage_risk {
                    upsert_risk(
                        document,
                        task_evidence_storage_risk(reason, document.evidence_epoch),
                    );
                }
                if snapshot_revision.is_some() && snapshot_revision != Some(document.revision) {
                    let current_gate = derive_completion_gate(
                        document,
                        self.evidence_path.as_deref(),
                        &desktop_activation_runtime,
                    );
                    if current_gate.status == TaskCompletionStatus::Blocked {
                        gate.status = TaskCompletionStatus::Blocked;
                    }
                    gate.reasons.extend(current_gate.reasons);
                    gate.reasons.sort();
                    gate.reasons.dedup();
                }
                document.completion = Some(gate.clone());
                document.updated_at = timestamp();
                document.revision = document.revision.saturating_add(1);
                document.clone()
            })
        };
        if let Some(snapshot) = snapshot {
            // The original write may have failed transiently. Persist the blocking
            // storage risk once more without recursing if storage remains unavailable.
            let _ = self.persist_document(&snapshot).await;
        }
        gate
    }

    async fn refresh_external_file_freshness(&self) {
        let _ = self
            .refresh_external_file_freshness_for(FreshnessPurpose::Ordinary)
            .await;
    }

    async fn refresh_external_file_freshness_for(&self, purpose: FreshnessPurpose) -> bool {
        for _ in 0..3 {
            if self.refresh_external_file_freshness_once(purpose).await {
                return true;
            }
        }
        false
    }

    fn lock_freshness_state(&self) -> std::sync::MutexGuard<'_, FreshnessState> {
        self.freshness_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Returns false only when ledger state changed during the filesystem scan.
    async fn refresh_external_file_freshness_once(&self, purpose: FreshnessPurpose) -> bool {
        if !self.allows_kd4_completion() {
            return true;
        }
        let Some(repo_root) = self.repo_root.as_deref() else {
            return true;
        };
        let manifest = {
            let guard = self.document.lock().await;
            guard.as_ref().map(freshness_manifest).unwrap_or_default()
        };
        if manifest.tracked.is_empty() && manifest.artifact_paths.is_empty() {
            return true;
        }
        let Ok(_freshness_permit) = self.freshness_gate.acquire().await else {
            return false;
        };
        let manifest_after_wait = {
            let guard = self.document.lock().await;
            guard.as_ref().map(freshness_manifest).unwrap_or_default()
        };
        if manifest_after_wait != manifest {
            return false;
        }
        if purpose == FreshnessPurpose::CompletionRetry
            && let Some(reused_hashes) = self.completion_proof_reuse_count(&manifest).await
        {
            let manifest_after_token_check = {
                let guard = self.document.lock().await;
                guard.as_ref().map(freshness_manifest).unwrap_or_default()
            };
            if manifest_after_token_check != manifest {
                return false;
            }
            self.lock_freshness_state().diagnostics.strong_hashes_reused += reused_hashes;
            return true;
        }
        self.lock_freshness_state().diagnostics.scan_invocations += 1;
        let scan = self
            .scan_freshness_manifest(repo_root, &manifest, purpose == FreshnessPurpose::Ordinary)
            .await;
        let manifest_after_scan = {
            let guard = self.document.lock().await;
            guard.as_ref().map(freshness_manifest).unwrap_or_default()
        };
        if manifest_after_scan != manifest {
            return false;
        }
        self.commit_freshness_cache(&scan);
        let expected = manifest.tracked.clone();
        let previous_artifacts = manifest.artifacts.clone();
        let mut changed = Vec::new();
        for (path, previous) in expected {
            let current = scan
                .snapshots
                .get(&path)
                .cloned()
                .unwrap_or_else(|| previous.clone());
            if current != previous {
                changed.push((previous, current));
            }
        }
        let mut current_artifacts = BTreeMap::new();
        let mut changed_artifacts = false;
        for path in &manifest.artifact_paths {
            let current = scan.snapshots.get(path).cloned().unwrap_or_else(|| {
                rejected_generated_artifact_snapshot(path, "FreshnessScanUnavailable")
            });
            changed_artifacts |= previous_artifacts.get(path) != Some(&current);
            current_artifacts.insert(path.clone(), current);
        }
        changed_artifacts |= previous_artifacts.len() != current_artifacts.len();
        if changed.is_empty() && !changed_artifacts {
            if purpose != FreshnessPurpose::Ordinary {
                self.store_completion_proof(manifest, scan);
            }
            return true;
        }

        let Some((committed, snapshot)) = self
            .update_document(|document| {
                if freshness_manifest(document) != manifest {
                    return false;
                }
                let changed = changed
                    .into_iter()
                    .filter(|(previous, current)| {
                        document.latest_file_hashes.get(&current.path) == Some(previous)
                    })
                    .map(|(_, current)| current)
                    .collect::<Vec<_>>();
                let artifact_state_is_current =
                    document.latest_generated_artifact_hashes == previous_artifacts;
                if changed.is_empty() && (!changed_artifacts || !artifact_state_is_current) {
                    return false;
                }
                let prior_artifact_state_exists = changed_artifacts
                    && !previous_artifacts.is_empty()
                    && artifact_state_is_current;
                let changed_files = !changed.is_empty();
                if changed_files || prior_artifact_state_exists {
                    let changed_paths = changed
                        .iter()
                        .map(|current| current.path.clone())
                        .collect::<BTreeSet<_>>();
                    let affected_paths = (!prior_artifact_state_exists).then_some(&changed_paths);
                    invalidate_for_mutation(document, affected_paths);
                }
                let epoch = document.evidence_epoch;
                for current in changed {
                    let path = current.path.clone();
                    if current.read_error.is_some() {
                        upsert_risk(document, unreadable_file_risk(&path, epoch, "freshness"));
                    } else {
                        resolve_risk(document, &unreadable_file_risk_id(&path));
                    }
                    document.latest_file_hashes.insert(path.clone(), current);
                }
                document.latest_generated_artifact_hashes = current_artifacts;
                if changed_files || prior_artifact_state_exists {
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("external-change-{epoch}"),
                            description: "a task-controlled file changed after its recorded state"
                                .to_string(),
                            source: "freshness".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
                }
                document.updated_at = timestamp();
                document.completion = None;
                true
            })
            .await
        else {
            return true;
        };
        if !committed {
            return false;
        }
        if purpose == FreshnessPurpose::Ordinary {
            self.lock_freshness_state().completion_proof = None;
        } else {
            self.store_completion_proof(freshness_manifest(&snapshot), scan);
        }
        self.persist_document(&snapshot).await;
        true
    }

    async fn scan_freshness_manifest(
        &self,
        repo_root: &Path,
        manifest: &FreshnessManifest,
        allow_cache_reuse: bool,
    ) -> FreshnessScanResult {
        let mut requests = BTreeMap::<PathBuf, BTreeSet<String>>::new();
        let mut snapshots = BTreeMap::new();
        for path in manifest.tracked.keys() {
            requests
                .entry(lexical_absolute_path(repo_root, Path::new(path)))
                .or_default()
                .insert(path.clone());
        }
        for path in &manifest.artifact_paths {
            match validated_generated_artifact_path(repo_root, path) {
                Ok(absolute) => {
                    requests.entry(absolute).or_default().insert(path.clone());
                }
                Err(snapshot) => {
                    snapshots.insert(path.clone(), snapshot);
                }
            }
        }

        #[cfg(test)]
        let before_scan = { self.lock_freshness_state().before_next_scan.take() };
        #[cfg(test)]
        if let Some((started, release)) = before_scan {
            started.wait().await;
            release.wait().await;
        }

        let request_count = requests.len();
        let mut tokens = BTreeMap::new();
        let mut cache_updates = BTreeMap::new();
        let mut cache_removals = BTreeSet::new();
        for (absolute, associations) in requests {
            let observation = self
                .observe_freshness_file(&absolute, allow_cache_reuse)
                .await;
            if let (Some(token), Some(sha1)) = (
                observation.token.as_ref(),
                observation.snapshot.sha1.as_ref(),
            ) {
                tokens.insert(absolute.clone(), token.clone());
                cache_updates.insert(
                    absolute.clone(),
                    CachedStrongHash {
                        token: token.clone(),
                        sha1: sha1.clone(),
                    },
                );
            } else {
                cache_removals.insert(absolute.clone());
            }
            for path in associations {
                let mut snapshot = observation.snapshot.clone();
                snapshot.path = normalize_slashes(&path);
                snapshots.insert(path, snapshot);
            }
        }
        let all_reusable = tokens.len() == request_count;
        FreshnessScanResult {
            snapshots,
            tokens,
            cache_updates,
            cache_removals,
            all_reusable,
        }
    }

    fn commit_freshness_cache(&self, scan: &FreshnessScanResult) {
        let mut state = self.lock_freshness_state();
        for path in &scan.cache_removals {
            state.cache.remove(path);
        }
        state.cache.extend(scan.cache_updates.clone());
        debug!(
            freshness_scan_invocations = state.diagnostics.scan_invocations,
            freshness_files_strongly_hashed = state.diagnostics.files_strongly_hashed,
            freshness_bytes_strongly_hashed = state.diagnostics.bytes_strongly_hashed,
            freshness_strong_hashes_reused = state.diagnostics.strong_hashes_reused,
            "task-evidence freshness diagnostics"
        );
    }

    async fn observe_freshness_file(
        &self,
        absolute: &Path,
        allow_cache_reuse: bool,
    ) -> FreshnessPathObservation {
        let display_path = normalize_slashes(&absolute.to_string_lossy());
        let mut file = match tokio::fs::File::open(absolute).await {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return FreshnessPathObservation {
                    snapshot: FileHashSnapshot {
                        path: display_path,
                        sha1: None,
                        exists: false,
                        read_error: None,
                    },
                    token: None,
                };
            }
            Err(err) => {
                return FreshnessPathObservation {
                    snapshot: FileHashSnapshot {
                        path: display_path,
                        sha1: None,
                        exists: tokio::fs::symlink_metadata(absolute).await.is_ok(),
                        read_error: Some(format!("{:?}", err.kind())),
                    },
                    token: None,
                };
            }
        };
        #[cfg(test)]
        let (force_untrusted_tokens, force_ambiguous_tokens) = {
            let state = self.lock_freshness_state();
            (state.force_untrusted_tokens, state.force_ambiguous_tokens)
        };
        #[cfg(not(test))]
        let force_untrusted_tokens = false;
        let token_before = if force_untrusted_tokens {
            None
        } else {
            trusted_file_token(&file).await
        };
        let cached =
            allow_cache_reuse.then(|| self.lock_freshness_state().cache.get(absolute).cloned());
        if let (Some(token), Some(Some(cached))) = (token_before.as_ref(), cached)
            && cached.token == *token
        {
            self.lock_freshness_state().diagnostics.strong_hashes_reused += 1;
            return FreshnessPathObservation {
                snapshot: FileHashSnapshot {
                    path: display_path,
                    sha1: Some(cached.sha1),
                    exists: true,
                    read_error: None,
                },
                token: Some(token.clone()),
            };
        }

        let mut hasher = Sha1::new();
        let mut buffer = vec![0_u8; FILE_HASH_CHUNK_SIZE];
        let mut bytes_hashed = 0_u64;
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(bytes_read) => {
                    hasher.update(&buffer[..bytes_read]);
                    bytes_hashed += bytes_read as u64;
                }
                Err(err) => {
                    return FreshnessPathObservation {
                        snapshot: FileHashSnapshot {
                            path: display_path,
                            sha1: None,
                            exists: true,
                            read_error: Some(format!("{:?}", err.kind())),
                        },
                        token: None,
                    };
                }
            }
        }
        {
            let mut state = self.lock_freshness_state();
            state.diagnostics.files_strongly_hashed += 1;
            state.diagnostics.bytes_strongly_hashed += bytes_hashed;
        }
        let sha1 = format!("{:x}", hasher.finalize());
        let token_after = if force_untrusted_tokens {
            None
        } else {
            trusted_file_token(&file).await
        };
        #[cfg(test)]
        let token_after = if force_ambiguous_tokens {
            token_after.map(|mut token| {
                token.len = token.len.wrapping_add(1);
                token
            })
        } else {
            token_after
        };
        let stable_token = token_before.filter(|before| token_after.as_ref() == Some(before));
        FreshnessPathObservation {
            snapshot: FileHashSnapshot {
                path: display_path,
                sha1: Some(sha1),
                exists: true,
                read_error: None,
            },
            token: stable_token,
        }
    }

    async fn completion_proof_reuse_count(&self, manifest: &FreshnessManifest) -> Option<u64> {
        let proof = self.lock_freshness_state().completion_proof.clone();
        let proof = proof?;
        if !proof.all_reusable || proof.manifest != *manifest {
            return None;
        }
        for (path, expected) in &proof.tokens {
            let Ok(file) = tokio::fs::File::open(path).await else {
                return None;
            };
            if trusted_file_token(&file).await.as_ref() != Some(expected) {
                return None;
            }
        }
        Some(proof.tokens.len() as u64)
    }

    fn completion_proof_manifest(&self) -> Option<FreshnessManifest> {
        self.lock_freshness_state()
            .completion_proof
            .as_ref()
            .map(|proof| proof.manifest.clone())
    }

    fn store_completion_proof(&self, manifest: FreshnessManifest, scan: FreshnessScanResult) {
        self.lock_freshness_state().completion_proof = Some(CompletionProof {
            manifest,
            tokens: scan.tokens,
            all_reusable: scan.all_reusable,
        });
    }

    pub(crate) fn freshness_diagnostics(&self) -> FreshnessDiagnostics {
        self.lock_freshness_state().diagnostics
    }

    #[cfg(test)]
    pub(crate) fn install_freshness_scan_barrier(
        &self,
    ) -> (Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>) {
        let started = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        self.lock_freshness_state().before_next_scan =
            Some((Arc::clone(&started), Arc::clone(&release)));
        (started, release)
    }

    #[cfg(test)]
    pub(crate) fn set_force_untrusted_freshness_tokens(&self, force: bool) {
        self.lock_freshness_state().force_untrusted_tokens = force;
    }

    #[cfg(test)]
    pub(crate) fn set_force_ambiguous_freshness_tokens(&self, force: bool) {
        self.lock_freshness_state().force_ambiguous_tokens = force;
    }

    pub(crate) async fn reconcile_default_child_workspace_events(
        &self,
        events: &[codex_agent_task_store::WorkspaceEvent],
        root_session_id: &str,
        same_root_typed_actor_ids: &BTreeSet<String>,
    ) -> bool {
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return true;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        let (cursor, stored_scope_identity) = {
            let guard = self.document.lock().await;
            let ledger = guard
                .as_ref()
                .and_then(|document| document.completion_review_v2.as_ref());
            (
                ledger
                    .map(|ledger| ledger.last_workspace_event_epoch)
                    .unwrap_or_default(),
                ledger
                    .map(|ledger| ledger.workspace_proof_scope_identity.clone())
                    .unwrap_or_default(),
            )
        };
        let scanned = events
            .iter()
            .filter(|event| event.epoch > cursor)
            .cloned()
            .collect::<Vec<_>>();
        let current_scope_identity = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(workspace_proof_scope)
                .map(|scope| scope.identity)
                .unwrap_or_default()
        };
        if scanned.is_empty() && stored_scope_identity == current_scope_identity {
            return true;
        }
        let max_epoch = scanned
            .iter()
            .map(|event| event.epoch)
            .max()
            .unwrap_or(cursor);
        let legacy_actor_prefix = format!("legacy:{root_session_id}:");
        let root_actor_id = format!("root:{root_session_id}");
        let accepted = scanned
            .iter()
            .filter(|event| {
                workspace_event_actor_is_admitted(
                    event,
                    &root_actor_id,
                    &legacy_actor_prefix,
                    same_root_typed_actor_ids,
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut snapshots = BTreeMap::new();
        for event in &scanned {
            for path in &event.paths {
                if path == codex_agent_task_store::REPOSITORY_WIDE_PATH {
                    continue;
                }
                snapshots
                    .entry(path.clone())
                    .or_insert(snapshot_file(repo_root, path).await);
            }
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                let Some(existing_ledger) = document.completion_review_v2.as_ref() else {
                    return;
                };
                if existing_ledger.last_workspace_event_epoch != cursor {
                    return;
                }
                let scope = workspace_proof_scope(document);
                let latest_file_hashes = document.latest_file_hashes.clone();
                let Some(ledger) = document.completion_review_v2.as_mut() else {
                    return;
                };
                ledger.last_workspace_event_epoch = max_epoch;
                let scope_changed = ledger.workspace_proof_scope_identity != scope.identity;
                if scanned
                    .first()
                    .is_some_and(|event| event.epoch > cursor.saturating_add(1))
                {
                    ledger.workspace_event_history_complete = false;
                }
                for event in &scanned {
                    let actor_id = event.actor_id.clone().unwrap_or_default();
                    if ledger.attributed_workspace_events.iter().any(|stored| {
                        stored.workspace_id == event.workspace_id
                            && stored.epoch == event.epoch
                            && stored.actor_id == actor_id
                    }) {
                        continue;
                    }
                    let mut paths = event.paths.clone();
                    paths.sort();
                    paths.dedup();
                    let mut contracts = event.contracts.clone();
                    contracts.sort();
                    contracts.dedup();
                    ledger
                        .attributed_workspace_events
                        .push(TaskAttributedWorkspaceEvent {
                            workspace_id: event.workspace_id.clone(),
                            epoch: event.epoch,
                            actor_id,
                            paths,
                            contracts,
                            actor_kind: Some(event.actor_kind),
                            attribution_confidence: Some(event.attribution_confidence),
                            relevance: WorkspaceEventRelevance::Unknown,
                            classified_scope_identity: String::new(),
                        });
                }
                if ledger.attributed_workspace_events.len() > MAX_ATTRIBUTED_WORKSPACE_EVENTS {
                    ledger.workspace_event_history_complete = false;
                    trim_to_last(
                        &mut ledger.attributed_workspace_events,
                        MAX_ATTRIBUTED_WORKSPACE_EVENTS,
                    );
                }

                let mut relevant_paths = BTreeSet::new();
                let mut invalidating_fact = false;
                let mut unknown_fact = workspace_scope_history_is_unknown(
                    scope_changed,
                    ledger.workspace_event_history_complete,
                );
                let mut only_after_agent = true;
                let mut invalidating_epochs = Vec::new();
                for event in &mut ledger.attributed_workspace_events {
                    let requires_classification = scope_changed
                        || event.classified_scope_identity != scope.identity;
                    if !requires_classification {
                        continue;
                    }
                    event.relevance = classify_workspace_event(event, &scope);
                    event.classified_scope_identity.clone_from(&scope.identity);
                    match event.relevance {
                        WorkspaceEventRelevance::Relevant => {
                            let already_accounted_direct_mutation = matches!(
                                event.attribution_confidence,
                                Some(codex_agent_task_store::AttributionConfidence::Definitive)
                            ) && !event.paths.iter().any(|path| {
                                path == codex_agent_task_store::REPOSITORY_WIDE_PATH
                            }) && event.paths.iter().all(|path| {
                                latest_file_hashes.get(path) == snapshots.get(path)
                            });
                            if !already_accounted_direct_mutation {
                                invalidating_fact = true;
                                invalidating_epochs.push(event.epoch);
                                only_after_agent &= event.contracts.iter().any(|contract| {
                                    contract == "kd4-completion-review-afteragent"
                                });
                            }
                            relevant_paths.extend(
                                event
                                    .paths
                                    .iter()
                                    .filter(|path| {
                                        path.as_str()
                                            != codex_agent_task_store::REPOSITORY_WIDE_PATH
                                    })
                                    .cloned(),
                            );
                        }
                        WorkspaceEventRelevance::Unknown => {
                            invalidating_fact = true;
                            unknown_fact = true;
                            only_after_agent = false;
                            invalidating_epochs.push(event.epoch);
                        }
                        WorkspaceEventRelevance::Unrelated => {}
                    }
                }
                ledger.workspace_proof_scope_identity = scope.identity;

                if invalidating_fact || unknown_fact {
                    if only_after_agent && !unknown_fact {
                        invalidate_for_after_agent_mutation(document);
                    } else if unknown_fact {
                        invalidate_for_mutation(document, None);
                    } else {
                        invalidate_for_mutation(document, Some(&relevant_paths));
                    }
                }
                for event in &accepted {
                    if event.paths.iter().any(|path| {
                        relevant_paths
                            .iter()
                            .any(|controlled| validation_paths_overlap(controlled, path))
                    }) {
                        for path in &event.paths {
                            if let Some(state) = snapshots.get(path) {
                                document.latest_file_hashes.insert(path.clone(), state.clone());
                            }
                        }
                    }
                }
                if unknown_fact {
                    let epoch = document.evidence_epoch;
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("unattributed-workspace-mutation-{epoch}"),
                            description: format!(
                                "workspace mutation history was relevant or insufficiently attributed for the current proof scope (workspace epochs: {})",
                                invalidating_epochs
                                    .iter()
                                    .map(u64::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            source: "workspace-event".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
                }
                document.updated_at = timestamp();
            })
            .await
        else {
            return false;
        };
        self.persist_document(&snapshot).await == PersistOutcome::Persisted
    }

    async fn record_host_mutation(&self) {
        if self.mode == TaskEvidenceMode::Disabled {
            return;
        }
        let Some((_, snapshot)) = self
            .update_document(|document| {
                invalidate_for_mutation(document, None);
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        if self.persist_document(&snapshot).await == PersistOutcome::Failed {
            warn!("failed to persist task-evidence host mutation revision");
        }
    }

    #[cfg(test)]
    pub(crate) async fn record_external_mcp_evidence(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
    ) -> ExternalEvidenceCapture {
        self.record_external_mcp_evidence_with_limit(
            server_name,
            tool_name,
            call_id,
            result,
            None,
            None,
            MAX_EXTERNAL_EVIDENCE_RECEIPTS,
        )
        .await
    }

    pub(crate) async fn record_external_mcp_evidence_bound_with_provenance(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
    ) -> ExternalEvidenceCapture {
        self.record_external_mcp_evidence_with_limit(
            server_name,
            tool_name,
            call_id,
            result,
            provenance,
            implementation_identity_hash,
            MAX_EXTERNAL_EVIDENCE_RECEIPTS,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_external_mcp_evidence_with_limit(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
        max_receipts: usize,
    ) -> ExternalEvidenceCapture {
        if self.mode == TaskEvidenceMode::Disabled {
            return ExternalEvidenceCapture::Ignored;
        }
        let metadata = match extract_external_evidence_metadata(result) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => return ExternalEvidenceCapture::Ignored,
            Err(message) => return ExternalEvidenceCapture::Warning(message),
        };
        let Some(evidence_path) = self.evidence_path.clone() else {
            return ExternalEvidenceCapture::Warning(
                "external evidence persistence is unavailable for this task",
            );
        };
        let external_evidence_permit = match Arc::clone(&self.external_evidence_gate)
            .acquire_owned()
            .await
        {
            Ok(permit) => permit,
            Err(err) => {
                warn!("external evidence serialization gate unexpectedly closed: {err}");
                return ExternalEvidenceCapture::Warning(
                    "external evidence persistence is unavailable for this task",
                );
            }
        };
        let canonical_payload = canonical_mcp_result_payload(result);
        let canonical_bytes = match serde_json::to_vec(&canonical_payload) {
            Ok(bytes) => bytes,
            Err(_) => {
                return ExternalEvidenceCapture::Warning(
                    "external evidence result could not be canonicalized",
                );
            }
        };
        let result_sha256 = format!("{:x}", Sha256::digest(&canonical_bytes));
        let (
            task_epoch,
            step_id,
            step_revision,
            work_unit_id,
            attribution,
            workspace_root_fingerprint,
            host_mutation_revision,
        ) = {
            let mut guard = self.document.lock().await;
            let Some(document) = guard.as_mut() else {
                return ExternalEvidenceCapture::Ignored;
            };
            let (step_id, step_revision, work_unit_id, attribution) =
                current_action_attribution(document, "external_evidence", call_id);
            (
                document.evidence_epoch,
                step_id,
                step_revision,
                work_unit_id,
                attribution,
                workspace_root_fingerprint(&document.start),
                Some(document.host_mutation_revision),
            )
        };

        let (payload, payload_artifact_id, mut pending_artifact) = if canonical_bytes.len()
            <= EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES
        {
            (Some(canonical_payload), None, None)
        } else {
            let Some(codex_home) = self.codex_home.as_deref() else {
                return ExternalEvidenceCapture::Warning(
                    "external evidence payload storage is unavailable",
                );
            };
            let Some(thread_id) = self.thread_id.as_deref() else {
                return ExternalEvidenceCapture::Warning(
                    "external evidence thread identity is unavailable",
                );
            };
            let Some(artifact_bytes) = encode_external_evidence_artifact(&canonical_bytes) else {
                return ExternalEvidenceCapture::Warning(
                    "external evidence payload artifact could not be encoded",
                );
            };
            let pending = crate::tools::command_output_artifact::create_evidence_output_artifact(
                codex_home,
                thread_id,
                &artifact_bytes,
            )
            .await;
            let pending = match pending {
                Ok(pending) => pending,
                Err(err) => {
                    warn!("external evidence payload artifact could not be stored: {err}");
                    return ExternalEvidenceCapture::Warning(
                        "external evidence payload artifact could not be stored",
                    );
                }
            };
            let artifact_id = pending.id().to_string();
            let summary = serde_json::json!({
                "evidenceMetaSummary": {
                    "producer": metadata.producer.clone(),
                    "schemaVersion": metadata.producer_schema_version,
                    "payloadCompleteness": evidence_completeness_name(
                        metadata.payload_completeness
                    ),
                },
                "structuredFieldCount": result
                    .structured_content
                    .as_ref()
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len),
                "contentItems": result.content.len(),
                "isError": result.is_error,
                "artifact": {
                    "id": artifact_id,
                    "encoding": "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1"
                }
            });
            debug_assert!(
                serde_json::to_vec(&summary)
                    .is_ok_and(|bytes| { bytes.len() <= EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES }),
                "external evidence summary must remain within the inline payload cap"
            );
            (Some(summary), Some(artifact_id), Some(pending))
        };

        let document = Arc::clone(&self.document);
        let persistence_gate = Arc::clone(&self.persistence_gate);
        let last_persisted_revision = Arc::clone(&self.last_persisted_revision);
        let persistence_test_control = self.persistence_test_control();
        let codex_home = self.codex_home.clone();
        let thread_id = self.thread_id.clone();
        let mode = self.mode;
        let server_name = server_name.to_string();
        let tool_name = tool_name.to_string();
        let call_id = call_id.to_string();
        let tool_success = result.is_error != Some(true);
        let provenance = provenance.cloned();
        let implementation_identity_hash = implementation_identity_hash.map(str::to_string);
        let coordinator = tokio::spawn(async move {
            let _external_evidence_permit = external_evidence_permit;
            let mut persistence_permit = match Arc::clone(&persistence_gate).acquire_owned().await {
                Ok(permit) => permit,
                Err(err) => {
                    warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                    return ExternalEvidenceCapture::Warning(
                        "external evidence receipt could not be durably persisted",
                    );
                }
            };
            let mut receipt_id = String::new();
            let mut trimmed_receipts = Vec::new();
            let snapshot = {
                let mut guard = document.lock().await;
                let Some(document) = guard.as_mut() else {
                    return ExternalEvidenceCapture::Ignored;
                };
                let id = next_receipt_id(
                    "external-evidence",
                    &mut document.next_external_evidence_receipt_sequence,
                );
                receipt_id.clone_from(&id);
                let receipt = ExternalEvidenceReceipt {
                    id,
                    producer: metadata.producer,
                    producer_schema_version: metadata.producer_schema_version,
                    server_name,
                    tool_name,
                    call_id,
                    source_thread_id: provenance
                        .as_ref()
                        .map(|provenance| provenance.source_thread_id.clone()),
                    source_agent_path: provenance
                        .as_ref()
                        .map(|provenance| provenance.source_agent_path.clone()),
                    recorded_at: timestamp(),
                    task_epoch,
                    step_id,
                    step_revision,
                    work_unit_id,
                    attribution: Some(attribution),
                    workspace_root_fingerprint,
                    host_mutation_revision,
                    implementation_identity_hash,
                    provider_snapshot: metadata.provider_snapshot,
                    tool_success,
                    payload_completeness: metadata.payload_completeness,
                    truncated: metadata.truncated,
                    approximate: metadata.approximate,
                    limitations: metadata.limitations,
                    result_sha256,
                    payload,
                    payload_artifact_id,
                };
                accept_matching_external_proof(document, &receipt);
                document.external_evidence.push(receipt);
                let trim_count = document
                    .external_evidence
                    .len()
                    .saturating_sub(max_receipts);
                trimmed_receipts.extend(document.external_evidence.drain(..trim_count));
                document.updated_at = timestamp();
                document.revision = document.revision.saturating_add(1);
                document.clone()
            };
            let (outcome, returned_permit) = persist_document_with_permit(
                evidence_path.clone(),
                snapshot,
                persistence_permit,
                Arc::clone(&last_persisted_revision),
                persistence_test_control.clone(),
            )
            .await;
            persistence_permit = match returned_permit {
                Some(permit) => permit,
                None => match Arc::clone(&persistence_gate).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        return ExternalEvidenceCapture::Warning(
                            "external evidence receipt could not be durably persisted",
                        );
                    }
                },
            };
            match outcome {
                PersistOutcome::Persisted | PersistOutcome::Superseded => {
                    if let Some(pending) = pending_artifact.take() {
                        let _ = pending.mark_durable();
                    }
                    drop(persistence_permit);
                    for receipt in trimmed_receipts {
                        delete_external_artifact_owned(
                            codex_home.as_deref(),
                            thread_id.as_deref(),
                            receipt.payload_artifact_id.as_deref(),
                        )
                        .await;
                    }
                    ExternalEvidenceCapture::Stored
                }
                PersistOutcome::Failed => {
                    let restored_receipts = trimmed_receipts;
                    let failure_snapshot = {
                        let mut guard = document.lock().await;
                        let Some(document) = guard.as_mut() else {
                            drop(pending_artifact.take());
                            return ExternalEvidenceCapture::Warning(
                                "external evidence receipt could not be durably persisted",
                            );
                        };
                        let mut retained = std::mem::take(&mut document.external_evidence);
                        retained.retain(|receipt| receipt.id != receipt_id);
                        let mut restored = restored_receipts;
                        restored.append(&mut retained);
                        document.external_evidence = restored;
                        if mode == TaskEvidenceMode::Kd4Completion {
                            upsert_risk(
                                document,
                                task_evidence_storage_risk(
                                    "external evidence receipt could not be durably persisted",
                                    document.evidence_epoch,
                                ),
                            );
                        }
                        document.updated_at = timestamp();
                        document.revision = document.revision.saturating_add(1);
                        document.clone()
                    };
                    drop(pending_artifact.take());
                    let rollback_revision = failure_snapshot.revision;
                    let (rollback_outcome, _) = persist_document_with_permit(
                        evidence_path,
                        failure_snapshot,
                        persistence_permit,
                        Arc::clone(&last_persisted_revision),
                        persistence_test_control,
                    )
                    .await;
                    if rollback_outcome == PersistOutcome::Failed {
                        last_persisted_revision.fetch_max(rollback_revision, Ordering::AcqRel);
                    }
                    ExternalEvidenceCapture::Warning(
                        "external evidence receipt could not be durably persisted",
                    )
                }
            }
        });
        match coordinator.await {
            Ok(capture) => capture,
            Err(err) => {
                warn!("external evidence persistence coordinator failed: {err}");
                ExternalEvidenceCapture::Warning(
                    "external evidence receipt could not be durably persisted",
                )
            }
        }
    }

    async fn document_revision(&self) -> Option<u64> {
        self.document
            .lock()
            .await
            .as_ref()
            .map(|document| document.revision)
    }

    async fn atomic_review_update<T: Send>(
        &self,
        expected_revision: u64,
        expected_implementation_identity: Option<&str>,
        expected_dossier_snapshot: Option<&str>,
        update: impl FnOnce(&mut TaskEvidenceDocument) -> T + Send,
    ) -> AtomicReviewTransition<T> {
        self.atomic_review_update_with_commit(
            expected_revision,
            expected_implementation_identity,
            expected_dossier_snapshot,
            update,
            || {},
        )
        .await
    }

    async fn atomic_review_update_with_commit<T: Send>(
        &self,
        expected_revision: u64,
        expected_implementation_identity: Option<&str>,
        expected_dossier_snapshot: Option<&str>,
        update: impl FnOnce(&mut TaskEvidenceDocument) -> T + Send,
        commit: impl FnOnce() + Send,
    ) -> AtomicReviewTransition<T> {
        let Some(path) = self.evidence_path.clone() else {
            return AtomicReviewTransition::Failed;
        };
        let _permit = match Arc::clone(&self.persistence_gate).acquire_owned().await {
            Ok(permit) => permit,
            Err(err) => {
                warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                return AtomicReviewTransition::Failed;
            }
        };
        let mut guard = Arc::clone(&self.document).lock_owned().await;
        let Some(document) = guard.as_ref() else {
            return AtomicReviewTransition::Failed;
        };
        if document.revision != expected_revision
            || !review_identity_is_current(
                document,
                expected_implementation_identity,
                expected_dossier_snapshot,
            )
        {
            return AtomicReviewTransition::Superseded;
        }

        let mut candidate = document.clone();
        let result = update(&mut candidate);
        candidate.schema_version = TASK_EVIDENCE_SCHEMA_VERSION;
        candidate.source_classification_cache = canonical_source_classification_cache(
            std::mem::take(&mut candidate.source_classification_cache),
        );
        candidate.revision = candidate.revision.saturating_add(1);
        let bytes = match serde_json::to_vec_pretty(&candidate) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("failed to serialize KD4 task evidence transition: {err}");
                return AtomicReviewTransition::Failed;
            }
        };
        let test_control = self.persistence_test_control();
        let write_result = tokio::task::spawn_blocking(move || {
            if let Some(control) = test_control.as_ref()
                && let Some((started, release)) = control
                    .before_next_write
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                started.wait();
                release.wait();
            }
            if test_control
                .as_ref()
                .is_some_and(|control| control.fail_writes.load(Ordering::Acquire))
            {
                Err(io::Error::other(
                    "injected task-evidence persistence failure",
                ))
            } else {
                atomic_write_evidence(&path, &bytes)
            }
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                self.last_persisted_revision
                    .store(candidate.revision, Ordering::Release);
                commit();
                *guard = Some(candidate);
                AtomicReviewTransition::Persisted(result)
            }
            Ok(Err(err)) => {
                warn!("failed to persist atomic KD4 completion-review transition: {err}");
                AtomicReviewTransition::Failed
            }
            Err(err) => {
                warn!("KD4 completion-review persistence task failed: {err}");
                AtomicReviewTransition::Failed
            }
        }
    }

    async fn update_document<T>(
        &self,
        update: impl FnOnce(&mut TaskEvidenceDocument) -> T,
    ) -> Option<(T, TaskEvidenceDocument)> {
        let mut guard = self.document.lock().await;
        let document = guard.as_mut()?;
        let result = update(document);
        document.revision = document.revision.saturating_add(1);
        Some((result, document.clone()))
    }

    async fn persist_document(&self, document: &TaskEvidenceDocument) -> PersistOutcome {
        let Some(path) = self.evidence_path.as_ref() else {
            return PersistOutcome::Persisted;
        };
        let permit = match Arc::clone(&self.persistence_gate).acquire_owned().await {
            Ok(permit) => permit,
            Err(err) => {
                warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                return PersistOutcome::Failed;
            }
        };
        persist_document_with_permit(
            path.clone(),
            document.clone(),
            permit,
            Arc::clone(&self.last_persisted_revision),
            self.persistence_test_control(),
        )
        .await
        .0
    }

    fn persistence_test_control(&self) -> Option<PersistenceTestControl> {
        #[cfg(test)]
        {
            return self
                .persistence_test_control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
        }
        #[cfg(not(test))]
        None
    }
}

fn completion_candidate_basis(
    document: &TaskEvidenceDocument,
    input: &FinalProofSealInputV1,
) -> CompletionCandidateBasisV1 {
    let mut child_gate_state = input.child_gate_state.clone();
    child_gate_state.sort();
    child_gate_state.dedup();
    let basis_id = canonical_hash(
        "KD4_COMPLETION_CANDIDATE_BASIS_V1",
        &serde_json::json!({
            "implementation_identity": input.implementation_identity,
            "source_identity": input.source_identity,
            "requirement_identity": input.requirement_identity,
            "task_evidence_epoch": document.evidence_epoch,
            "host_mutation_revision": document.host_mutation_revision,
            "workspace_epoch": input.workspace_epoch,
            "workspace_manifest_identity": input.workspace_manifest_identity,
            "environment_identity": input.environment_identity,
            "toolchain_identity": input.toolchain_identity,
            "features_identity": input.features_identity,
            "configuration_identity": input.configuration_identity,
            "child_gate_state": child_gate_state,
            "reviewer_configuration_identity": input.reviewer_configuration_identity,
            "canonical_diff_identity": input.diff_snapshot.diff_identity,
        }),
    );
    CompletionCandidateBasisV1 {
        basis_id,
        implementation_identity: input.implementation_identity.clone(),
        source_identity: input.source_identity.clone(),
        requirement_identity: input.requirement_identity.clone(),
        task_evidence_epoch: document.evidence_epoch,
        host_mutation_revision: document.host_mutation_revision,
        workspace_epoch: input.workspace_epoch,
        workspace_manifest_identity: input.workspace_manifest_identity.clone(),
        environment_identity: input.environment_identity.clone(),
        toolchain_identity: input.toolchain_identity.clone(),
        features_identity: input.features_identity.clone(),
        configuration_identity: input.configuration_identity.clone(),
        child_gate_state,
        reviewer_configuration_identity: input.reviewer_configuration_identity.clone(),
        canonical_diff_identity: input.diff_snapshot.diff_identity.clone(),
    }
}

fn reviewer_infrastructure_memo_identity(
    candidate_id: &str,
    dossier_id: &str,
    reviewer_configuration_identity: &str,
    infrastructure_condition_identity: &str,
) -> String {
    canonical_hash(
        "KD4_REVIEWER_INFRASTRUCTURE_MEMO_V1",
        &serde_json::json!({
            "candidate_id": candidate_id,
            "dossier_id": dossier_id,
            "reviewer_configuration_identity": reviewer_configuration_identity,
            "infrastructure_condition_identity": infrastructure_condition_identity,
        }),
    )
}

fn validation_plan_for_basis(
    document: &TaskEvidenceDocument,
    basis: &CompletionCandidateBasisV1,
) -> ValidationPlanV1 {
    let mut steps = Vec::new();
    let mut ambiguous_or_unmappable = false;
    for evidence_step in &document.plan {
        let needs_validation = evidence_step.validation_disposition
            != ValidationDisposition::NotRequired
            || evidence_step.validation_route.is_some();
        let Some(route) = evidence_step.validation_route.as_ref() else {
            ambiguous_or_unmappable |= needs_validation;
            continue;
        };
        for (leaf_index, leaf) in route.leaves.iter().enumerate() {
            let mut covered_paths = leaf.covered_paths.clone();
            covered_paths.sort();
            covered_paths.dedup();
            let mut covered_contracts = leaf.covered_contracts.clone();
            covered_contracts.sort();
            covered_contracts.dedup();
            steps.push(ValidationPlanStepV1 {
                step_id: format!("{}/validation/{}", evidence_step.id, leaf_index + 1),
                obligation_id: evidence_step.id.clone(),
                argv: leaf.argv.clone(),
                covered_paths,
                covered_contracts,
                timeout_ms: leaf.timeout_ms,
                semantic_timeout: leaf.semantic_timeout,
                batch_group: match route.ordering {
                    ValidationRouteOrdering::RunAll => 0,
                    ValidationRouteOrdering::StopOnFailure => {
                        u32::try_from(leaf_index + 1).unwrap_or(u32::MAX)
                    }
                },
            });
        }
    }
    if document.plan.is_empty()
        && let Some(work_unit) = document.planning.work_unit.as_ref()
    {
        let needs_validation = work_unit.validation_disposition
            != ValidationDisposition::NotRequired
            || work_unit.validation_route.is_some();
        if let Some(route) = work_unit.validation_route.as_ref() {
            for (leaf_index, leaf) in route.leaves.iter().enumerate() {
                let mut covered_paths = leaf.covered_paths.clone();
                covered_paths.sort();
                covered_paths.dedup();
                let mut covered_contracts = leaf.covered_contracts.clone();
                covered_contracts.sort();
                covered_contracts.dedup();
                steps.push(ValidationPlanStepV1 {
                    step_id: format!("{}/validation/{}", work_unit.id, leaf_index + 1),
                    obligation_id: work_unit.id.clone(),
                    argv: leaf.argv.clone(),
                    covered_paths,
                    covered_contracts,
                    timeout_ms: leaf.timeout_ms,
                    semantic_timeout: leaf.semantic_timeout,
                    batch_group: match route.ordering {
                        ValidationRouteOrdering::RunAll => 0,
                        ValidationRouteOrdering::StopOnFailure => {
                            u32::try_from(leaf_index + 1).unwrap_or(u32::MAX)
                        }
                    },
                });
            }
        } else {
            ambiguous_or_unmappable |= needs_validation;
        }
    }
    let plan_id = canonical_hash(
        "KD4_VALIDATION_PLAN_V1",
        &serde_json::json!({
            "basis_id": basis.basis_id,
            "steps": steps,
            "ambiguous_or_unmappable": ambiguous_or_unmappable,
            "resolution_generation_used": false,
        }),
    );
    ValidationPlanV1 {
        plan_id,
        basis_id: basis.basis_id.clone(),
        steps,
        ambiguous_or_unmappable,
        resolution_generation_used: false,
    }
}

fn completion_candidate_for(
    basis: &CompletionCandidateBasisV1,
    plan: &ValidationPlanV1,
) -> CompletionCandidateV1 {
    let lineage_id = canonical_hash(
        "KD4_COMPLETION_CANDIDATE_LINEAGE_V1",
        &serde_json::json!({
            "source_identity": basis.source_identity,
            "requirement_identity": basis.requirement_identity,
            "environment_identity": basis.environment_identity,
            "toolchain_identity": basis.toolchain_identity,
            "features_identity": basis.features_identity,
            "configuration_identity": basis.configuration_identity,
        }),
    );
    let candidate_id = canonical_hash(
        "KD4_COMPLETION_CANDIDATE_V1",
        &serde_json::json!({
            "basis_id": basis.basis_id,
            "validation_plan_id": plan.plan_id,
        }),
    );
    CompletionCandidateV1 {
        candidate_id,
        basis_id: basis.basis_id.clone(),
        validation_plan_id: plan.plan_id.clone(),
        lineage_id,
    }
}

fn current_final_proof_observations(
    document: &TaskEvidenceDocument,
    _basis: &CompletionCandidateBasisV1,
    candidate: &CompletionCandidateV1,
    plan: &ValidationPlanV1,
) -> (Vec<FinalProofObservationV1>, u32, u64) {
    let mut by_step = document
        .final_proof
        .proof_observations
        .iter()
        .filter(|observation| {
            observation.candidate_id == candidate.candidate_id
                && observation.evidence_revision == document.evidence_epoch
                && observation.complete_identity
        })
        .map(|observation| (observation.plan_step_id.clone(), observation.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut validation_receipt_ids = BTreeSet::new();
    let mut validation_process_ns = 0_u64;
    for step in &plan.steps {
        let Some(receipt) = document.command_receipts.iter().rev().find(|receipt| {
            crate::validation_admission::validation_argv_semantically_covers(
                &receipt.command,
                &step.argv,
            ) && receipt.exit_code == 0
                && !receipt.timed_out
                && !receipt.possible_mutation
                && command_receipt_has_current_proof_identity(document, receipt)
        }) else {
            continue;
        };
        if validation_receipt_ids.insert(receipt.id.clone()) {
            validation_process_ns =
                validation_process_ns.saturating_add(receipt.duration_ms.saturating_mul(1_000_000));
        }
        let invocation_identity = canonical_hash(
            "KD4_VALIDATION_INVOCATION_V1",
            &serde_json::json!({"argv": receipt.command, "cwd": receipt.cwd}),
        );
        let coverage_identity = canonical_hash(
            "KD4_VALIDATION_COVERAGE_V1",
            &serde_json::json!({
                "paths": step.covered_paths,
                "contracts": step.covered_contracts,
            }),
        );
        let retained_output_digest = canonical_hash(
            "KD4_VALIDATION_RECEIPT_V1",
            &serde_json::to_value(receipt).unwrap_or(Value::Null),
        );
        by_step.insert(
            step.step_id.clone(),
            FinalProofObservationV1 {
                candidate_id: candidate.candidate_id.clone(),
                plan_step_id: step.step_id.clone(),
                obligation_id: step.obligation_id.clone(),
                successful: true,
                complete_identity: true,
                invocation_identity,
                coverage_identity,
                retained_output_digest,
                retained_output_ref: Some(receipt.id.clone()),
                evidence_revision: document.evidence_epoch,
            },
        );
    }
    (
        by_step.into_values().collect(),
        u32::try_from(validation_receipt_ids.len()).unwrap_or(u32::MAX),
        validation_process_ns,
    )
}

fn missing_or_failed_obligations(
    candidate: &CompletionCandidateV1,
    plan: &ValidationPlanV1,
    observations: &[FinalProofObservationV1],
    evidence_revision: u64,
) -> Vec<String> {
    let successful_steps = observations
        .iter()
        .filter(|observation| {
            observation.candidate_id == candidate.candidate_id
                && observation.evidence_revision == evidence_revision
                && observation.complete_identity
                && observation.successful
        })
        .map(|observation| observation.plan_step_id.as_str())
        .collect::<BTreeSet<_>>();
    plan.steps
        .iter()
        .filter(|step| !successful_steps.contains(step.step_id.as_str()))
        .map(|step| step.obligation_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn completion_failure_fingerprint(
    correctness_evidence_revision: u64,
    candidate: &CompletionCandidateV1,
    missing_or_failed_obligation_ids: &[String],
    child_gate_state: &[String],
    reviewer_state: Option<String>,
) -> CompletionFailureFingerprintV1 {
    let fingerprint = canonical_hash(
        "KD4_COMPLETION_FAILURE_FINGERPRINT_V1",
        &serde_json::json!({
            "candidate_id": candidate.candidate_id,
            "correctness_evidence_revision": correctness_evidence_revision,
            "missing_or_failed_obligation_ids": missing_or_failed_obligation_ids,
            "child_gate_state": child_gate_state,
            "reviewer_state": reviewer_state,
        }),
    );
    CompletionFailureFingerprintV1 {
        fingerprint,
        candidate_id: candidate.candidate_id.clone(),
        correctness_evidence_revision,
        missing_or_failed_obligation_ids: missing_or_failed_obligation_ids.to_vec(),
        child_gate_state: child_gate_state.to_vec(),
        reviewer_state,
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_completion_reason(
    reasons: &mut Vec<CompletionReasonV1>,
    reason_code: &str,
    obligation_ids: &[String],
    path_ids: &[String],
    contract_ids: &[String],
    epoch: u64,
    latest_occurrence: &str,
    evidence_ref: Option<String>,
) {
    let mut obligation_ids = obligation_ids.to_vec();
    obligation_ids.sort();
    obligation_ids.dedup();
    let mut path_ids = path_ids.to_vec();
    path_ids.sort();
    path_ids.dedup();
    let mut contract_ids = contract_ids.to_vec();
    contract_ids.sort();
    contract_ids.dedup();
    if let Some(existing) = reasons.iter_mut().find(|reason| {
        reason.reason_code == reason_code
            && reason.obligation_ids == obligation_ids
            && reason.path_ids == path_ids
            && reason.contract_ids == contract_ids
    }) {
        existing.last_epoch = epoch;
        existing.occurrence_count = existing.occurrence_count.saturating_add(1);
        existing.latest_occurrence = latest_occurrence.to_string();
        existing.evidence_ref = evidence_ref;
        return;
    }
    reasons.push(CompletionReasonV1 {
        reason_code: reason_code.to_string(),
        obligation_ids,
        path_ids,
        contract_ids,
        first_epoch: epoch,
        last_epoch: epoch,
        occurrence_count: 1,
        latest_occurrence: latest_occurrence.to_string(),
        evidence_ref,
    });
    reasons.sort_by(|left, right| {
        left.reason_code
            .cmp(&right.reason_code)
            .then_with(|| left.obligation_ids.cmp(&right.obligation_ids))
            .then_with(|| left.path_ids.cmp(&right.path_ids))
            .then_with(|| left.contract_ids.cmp(&right.contract_ids))
    });
}

fn completion_checkpoint_for(
    document: &TaskEvidenceDocument,
    basis: &CompletionCandidateBasisV1,
    candidate: &CompletionCandidateV1,
    plan: &ValidationPlanV1,
    diff: &CandidateDiffSnapshotV1,
    observations: &[FinalProofObservationV1],
    token_budget: usize,
) -> Result<CompletionCheckpointV1, String> {
    let requirements = document
        .completion_review_v2
        .as_ref()
        .and_then(active_manifest)
        .map(|manifest| {
            manifest
                .requirements
                .iter()
                .filter(|requirement| requirement.status == RequirementStatus::Active)
                .map(|requirement| CheckpointRequirementV1 {
                    requirement_id: requirement.requirement_id.clone(),
                    source_id: requirement.source_id.clone(),
                    exact_text: requirement.exact_material.clone(),
                    status: "active".to_string(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut affected_surfaces = document
        .plan
        .iter()
        .flat_map(|step| {
            step.runtime_paths
                .iter()
                .chain(step.implementation_surfaces.iter())
                .chain(step.edit_paths.iter())
        })
        .cloned()
        .collect::<Vec<_>>();
    if let Some(work_unit) = document.planning.work_unit.as_ref() {
        affected_surfaces.extend(work_unit.implementation_surfaces.iter().cloned());
    }
    affected_surfaces.sort();
    affected_surfaces.dedup();
    let observations_by_step = observations
        .iter()
        .map(|observation| (observation.plan_step_id.as_str(), observation))
        .collect::<BTreeMap<_, _>>();
    let proof_receipts = plan
        .steps
        .iter()
        .map(|step| {
            let observation = observations_by_step.get(step.step_id.as_str()).copied();
            CheckpointProofReceiptV1 {
                obligation_id: step.obligation_id.clone(),
                status: if observation.is_some_and(|observation| {
                    observation.successful && observation.complete_identity
                }) {
                    "passed".to_string()
                } else {
                    "missing_or_stale".to_string()
                },
                evidence_ref: observation
                    .and_then(|observation| observation.retained_output_ref.clone()),
                evidence_digest: observation
                    .map(|observation| observation.retained_output_digest.clone())
                    .unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    let evidence_gate =
        derive_completion_gate(document, None, &DesktopActivationRuntimeSnapshot::default());
    let unresolved_blockers = evidence_gate.reasons;
    let unresolved_risks = document
        .risks
        .iter()
        .filter(|risk| !risk.resolved)
        .map(|risk| format!("{}: {}", risk.id, risk.description))
        .collect::<Vec<_>>();
    let mut evidence_artifact_references = observations
        .iter()
        .filter_map(|observation| observation.retained_output_ref.clone())
        .chain(
            document
                .external_evidence
                .iter()
                .filter_map(|receipt| receipt.payload_artifact_id.clone()),
        )
        .chain(diff.raw_artifact_ref.iter().cloned())
        .collect::<Vec<_>>();
    evidence_artifact_references.sort();
    evidence_artifact_references.dedup();
    let mut checkpoint = CompletionCheckpointV1 {
        checkpoint_id: String::new(),
        candidate_id: candidate.candidate_id.clone(),
        basis_id: basis.basis_id.clone(),
        validation_plan_id: plan.plan_id.clone(),
        diff_identity: diff.diff_identity.clone(),
        requirements,
        affected_surfaces,
        changed_paths: diff.changed_paths.clone(),
        bounded_hunks: truncate_checkpoint_hunks(
            &diff.bounded_hunks,
            MAX_COMPLETION_CHECKPOINT_HUNK_BYTES,
        ),
        proof_receipts,
        unresolved_blockers,
        unresolved_risks,
        child_gate_state: basis.child_gate_state.clone(),
        evidence_artifact_references,
        estimated_tokens: 0,
    };
    let mut mandatory = checkpoint.clone();
    mandatory.bounded_hunks.clear();
    let mandatory_payload = serde_json::to_string(&mandatory)
        .map_err(|_| "completion checkpoint could not be serialized".to_string())?;
    let mandatory_tokens = approx_token_count(&mandatory_payload);
    if token_budget == 0 || mandatory_tokens > token_budget {
        return Err(format!(
            "completion checkpoint mandatory material requires approximately {mandatory_tokens} tokens but only {token_budget} are available"
        ));
    }
    loop {
        let payload = serde_json::to_string(&checkpoint)
            .map_err(|_| "completion checkpoint could not be serialized".to_string())?;
        if approx_token_count(&payload) <= token_budget || checkpoint.bounded_hunks.is_empty() {
            break;
        }
        let target = checkpoint.bounded_hunks.len() / 2;
        checkpoint.bounded_hunks = truncate_checkpoint_hunks(&checkpoint.bounded_hunks, target);
    }
    checkpoint.checkpoint_id = canonical_hash(
        "KD4_COMPLETION_CHECKPOINT_V1",
        &serde_json::to_value(&checkpoint).unwrap_or(Value::Null),
    );
    checkpoint.estimated_tokens = checkpoint
        .canonical_payload()
        .map(|payload| u64::try_from(approx_token_count(&payload)).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX);
    Ok(checkpoint)
}

fn truncate_checkpoint_hunks(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

fn review_identity_is_current(
    document: &TaskEvidenceDocument,
    expected_implementation_identity: Option<&str>,
    expected_dossier_snapshot: Option<&str>,
) -> bool {
    if expected_implementation_identity.is_none() && expected_dossier_snapshot.is_none() {
        return true;
    }
    let Some(ledger) = document.completion_review_v2.as_ref() else {
        return false;
    };
    let accepted_review_id = ledger
        .active_review_cycle
        .as_ref()
        .and_then(|cycle| cycle.accepted_review_id.as_deref());
    let Some(receipt) = accepted_review_id.and_then(|review_id| {
        ledger
            .receipts
            .iter()
            .find(|receipt| receipt.review_id == review_id)
    }) else {
        return false;
    };
    expected_implementation_identity
        .is_none_or(|expected| receipt.implementation_identity_hash == expected)
        && expected_dossier_snapshot.is_none_or(|expected| receipt.dossier_snapshot_id == expected)
}

async fn user_source_material(
    input: &UserInput,
) -> Option<(UserSourceKind, String, UserSourceAvailability)> {
    #[allow(unreachable_patterns)]
    match input {
        UserInput::Text { text, .. } if !text.is_empty() => Some((
            UserSourceKind::Text,
            text.clone(),
            UserSourceAvailability::Available,
        )),
        UserInput::Image { image_url, .. } => Some((
            UserSourceKind::Image,
            image_url.clone(),
            if image_url.is_empty() {
                UserSourceAvailability::Unavailable
            } else {
                UserSourceAvailability::Available
            },
        )),
        UserInput::LocalImage { path, .. } => Some(
            file_backed_source_material(UserSourceKind::Image, "local-image", None, path).await,
        ),
        UserInput::Skill { name, path } => Some(
            file_backed_source_material(UserSourceKind::Attachment, "skill", Some(name), path)
                .await,
        ),
        UserInput::Mention { name, path } => Some((
            UserSourceKind::Attachment,
            format!("mention:{name}:{path}"),
            UserSourceAvailability::Available,
        )),
        _ => None,
    }
}

async fn file_backed_source_material(
    kind: UserSourceKind,
    reference_kind: &str,
    name: Option<&str>,
    path: &Path,
) -> (UserSourceKind, String, UserSourceAvailability) {
    let normalized_path = normalize_path_for_identity(path);
    let reference = match name {
        Some(name) => format!("{reference_kind}:{name}:{normalized_path}"),
        None => format!("{reference_kind}:{normalized_path}"),
    };
    match sha256_file(path).await {
        Ok(sha256) => (
            kind,
            format!("{reference}#sha256={sha256}"),
            UserSourceAvailability::Available,
        ),
        Err(_) => (
            kind,
            format!("{reference}#unavailable"),
            UserSourceAvailability::Unavailable,
        ),
    }
}

pub(crate) fn material_for_span(source: &UserSourceRecord, span: &SourceSpan) -> Option<String> {
    match (source.source_kind, span) {
        (UserSourceKind::Text, SourceSpan::Text { start, end })
            if start < end
                && *end <= source.exact_material.len()
                && source.exact_material.is_char_boundary(*start)
                && source.exact_material.is_char_boundary(*end) =>
        {
            source.exact_material.get(*start..*end).map(str::to_string)
        }
        (UserSourceKind::Image, SourceSpan::Image { reference, region })
            if reference == &source.exact_material =>
        {
            Some(match region {
                Some(region) if !region.trim().is_empty() => {
                    format!("{reference}#region={region}")
                }
                _ => reference.clone(),
            })
        }
        (UserSourceKind::Attachment, SourceSpan::Attachment { reference, range })
            if reference == &source.exact_material =>
        {
            Some(match range {
                Some(range) if !range.trim().is_empty() => format!("{reference}#range={range}"),
                _ => reference.clone(),
            })
        }
        _ => None,
    }
}

pub(crate) fn source_local_classifications_with_manifest_gaps(
    dossier: &CompletionReviewDossier,
    gaps: &[ManifestGapInput],
) -> Option<BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>> {
    if gaps.is_empty() {
        return None;
    }
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let existing = dossier
        .requirements
        .iter()
        .map(|requirement| ClassifiedRequirementRef {
            source_id: requirement.source_id.clone(),
            source_span: requirement.source_span.clone(),
        })
        .collect::<BTreeSet<_>>();
    let expected_keys = dossier
        .sources
        .iter()
        .map(source_classification_cache_key)
        .collect::<BTreeSet<_>>();
    let mut corrected = BTreeMap::new();
    for source in &dossier.sources {
        let key = source_classification_cache_key(source);
        let classification =
            if let Some(classification) = dossier.source_classification_cache.get(&key) {
                classification.clone()
            } else {
                let mut requirement_spans = dossier
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.source_id == source.source_id)
                    .map(|requirement| requirement.source_span.clone())
                    .collect::<Vec<_>>();
                requirement_spans.sort();
                requirement_spans.dedup();
                match dossier.source_mappings.get(&source.source_id)? {
                    SourceMapping::RequirementBearing { .. } if !requirement_spans.is_empty() => {
                        SourceLocalClassification {
                            local_kind: SourceLocalClassificationKind::RequirementBearing,
                            local_semantic_cues: requirement_spans
                                .iter()
                                .cloned()
                                .map(|source_span| LocalSemanticCue {
                                    kind: LocalSemanticCueKind::Assertion,
                                    source_span: Some(source_span),
                                })
                                .collect(),
                            requirement_spans,
                            reason:
                                "Reconstructed from the validated current requirement manifest."
                                    .to_string(),
                        }
                    }
                    SourceMapping::NonRequirement { reason } => SourceLocalClassification {
                        local_kind: SourceLocalClassificationKind::NonRequirement,
                        requirement_spans: Vec::new(),
                        local_semantic_cues: Vec::new(),
                        reason: reason.clone(),
                    },
                    SourceMapping::SupersededContext { reason } => SourceLocalClassification {
                        local_kind: SourceLocalClassificationKind::RelationshipOnlyContext,
                        requirement_spans: Vec::new(),
                        local_semantic_cues: vec![LocalSemanticCue {
                            kind: LocalSemanticCueKind::RelationshipOnlyContext,
                            source_span: None,
                        }],
                        reason: reason.clone(),
                    },
                    SourceMapping::UnavailableOrTruncated => SourceLocalClassification {
                        local_kind: SourceLocalClassificationKind::UnavailableOrTruncated,
                        requirement_spans: Vec::new(),
                        local_semantic_cues: Vec::new(),
                        reason: "The immutable source was unavailable or truncated.".to_string(),
                    },
                    SourceMapping::PendingClassification
                    | SourceMapping::RequirementBearing { .. } => return None,
                }
            };
        if corrected
            .insert(key, classification.clone())
            .is_some_and(|existing| existing != classification)
        {
            return None;
        }
    }
    let mut seen = BTreeSet::new();
    const CORRECTION_REASON: &str =
        "Validated immutable manifest gap establishes an omitted requirement span.";
    for gap in gaps {
        let source = sources.get(gap.source_id.as_str())?;
        if gap.omitted_spans.is_empty() {
            return None;
        }
        for span in &gap.omitted_spans {
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: span.clone(),
            };
            material_for_span(source, span)?;
            if existing.contains(&reference) || !seen.insert(reference) {
                return None;
            }
            let key = source_classification_cache_key(source);
            let classification = corrected.get_mut(&key)?;
            classification.local_kind = SourceLocalClassificationKind::RequirementBearing;
            classification.requirement_spans.push(span.clone());
            classification.requirement_spans.sort();
            classification.requirement_spans.dedup();
            classification.local_semantic_cues.push(LocalSemanticCue {
                kind: LocalSemanticCueKind::Assertion,
                source_span: Some(span.clone()),
            });
            classification.local_semantic_cues.sort();
            classification.local_semantic_cues.dedup();
            if !classification.reason.contains(CORRECTION_REASON) {
                classification.reason =
                    format!("{} {CORRECTION_REASON}", classification.reason.trim())
                        .trim()
                        .to_string();
            }
        }
    }
    if corrected.len() != expected_keys.len()
        || dossier.sources.iter().any(|source| {
            corrected
                .get(&source_classification_cache_key(source))
                .is_none_or(|classification| {
                    !source_local_classification_is_valid_for_source(source, classification)
                })
        })
    {
        return None;
    }
    Some(corrected)
}

fn prepare_source_materialization(
    dossier: &CompletionReviewDossier,
    materialization: SourceMaterialization,
) -> Option<PreparedSourceMaterialization> {
    let SourceMaterialization {
        local_classifications,
        resolved_sources,
    } = materialization;
    let expected_keys = dossier
        .sources
        .iter()
        .map(source_classification_cache_key)
        .collect::<BTreeSet<_>>();
    if local_classifications.len() != expected_keys.len()
        || local_classifications
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_keys
        || resolved_sources.len() != dossier.sources.len()
        || resolved_sources
            .iter()
            .zip(&dossier.sources)
            .any(|(resolved, source)| resolved.source_id != source.source_id)
    {
        return None;
    }

    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut requirement_ids = BTreeMap::<ClassifiedRequirementRef, String>::new();
    for (resolved, source) in resolved_sources.iter().zip(&dossier.sources) {
        let local = local_classifications.get(&source_classification_cache_key(source))?;
        if !source_local_classification_is_valid_for_source(source, local) {
            return None;
        }
        let expected_kind = match local.local_kind {
            SourceLocalClassificationKind::RequirementBearing => {
                ClassifiedSourceKind::RequirementBearing
            }
            SourceLocalClassificationKind::NonRequirement => ClassifiedSourceKind::NonRequirement,
            SourceLocalClassificationKind::RelationshipOnlyContext => {
                ClassifiedSourceKind::SupersededContext
            }
            SourceLocalClassificationKind::UnavailableOrTruncated => {
                ClassifiedSourceKind::UnavailableOrTruncated
            }
        };
        let mut resolved_spans = resolved
            .requirements
            .iter()
            .map(|requirement| requirement.source_span.clone())
            .collect::<Vec<_>>();
        resolved_spans.sort();
        if resolved.kind != expected_kind
            || resolved_spans != local.requirement_spans
            || matches!(
                local.local_kind,
                SourceLocalClassificationKind::NonRequirement
                    | SourceLocalClassificationKind::RelationshipOnlyContext
            ) && resolved.reason.as_deref() != Some(local.reason.as_str())
        {
            return None;
        }
        for requirement in &resolved.requirements {
            material_for_span(source, &requirement.source_span)?;
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: requirement.source_span.clone(),
            };
            let requirement_id = deterministic_requirement_id(source, &requirement.source_span);
            if requirement_ids.insert(reference, requirement_id).is_some() {
                return None;
            }
        }
    }
    if resolved_sources.iter().any(|resolved| {
        resolved
            .requirements
            .iter()
            .any(|requirement| match requirement.status {
                RequirementStatus::Active | RequirementStatus::Withdrawn => {
                    requirement.superseded_by.is_some()
                }
                RequirementStatus::Superseded => requirement
                    .superseded_by
                    .as_ref()
                    .is_none_or(|target| !requirement_ids.contains_key(target)),
            })
    }) {
        return None;
    }

    let mut requirements = Vec::new();
    let mut mappings = Vec::new();
    for resolved in resolved_sources {
        let source = sources.get(resolved.source_id.as_str())?;
        let mut mapped_requirement_ids = Vec::new();
        for requirement in resolved.requirements {
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: requirement.source_span.clone(),
            };
            let requirement_id = requirement_ids.get(&reference)?.clone();
            let exact_material = material_for_span(source, &requirement.source_span)?;
            mapped_requirement_ids.push(requirement_id.clone());
            requirements.push(RequirementRecord {
                requirement_id,
                source_id: source.source_id.clone(),
                source_content_hash: source.content_hash.clone(),
                exact_material,
                source_span: requirement.source_span,
                status: requirement.status,
                superseded_by: requirement
                    .superseded_by
                    .as_ref()
                    .and_then(|target| requirement_ids.get(target))
                    .cloned(),
            });
        }
        mapped_requirement_ids.sort();
        let mapping = match resolved.kind {
            ClassifiedSourceKind::RequirementBearing => SourceMapping::RequirementBearing {
                requirement_ids: mapped_requirement_ids,
            },
            ClassifiedSourceKind::NonRequirement => SourceMapping::NonRequirement {
                reason: resolved.reason.unwrap_or_default(),
            },
            ClassifiedSourceKind::SupersededContext => SourceMapping::SupersededContext {
                reason: resolved.reason.unwrap_or_default(),
            },
            ClassifiedSourceKind::UnavailableOrTruncated => SourceMapping::UnavailableOrTruncated,
        };
        mappings.push((source.source_id.clone(), mapping));
    }
    requirements.sort_by(|left, right| left.requirement_id.cmp(&right.requirement_id));
    if !requirement_supersession_is_acyclic(&requirements) {
        return None;
    }
    let next_requirements = requirements
        .iter()
        .map(|requirement| (requirement.requirement_id.as_str(), requirement))
        .collect::<BTreeMap<_, _>>();
    for previous in &dossier.requirements {
        let next = next_requirements.get(previous.requirement_id.as_str())?;
        if next.source_id != previous.source_id
            || next.source_content_hash != previous.source_content_hash
            || next.source_span != previous.source_span
            || next.exact_material != previous.exact_material
            || dossier.relationship_resolution_current
                && match previous.status {
                    RequirementStatus::Active => false,
                    RequirementStatus::Superseded => {
                        next.status != RequirementStatus::Superseded
                            || next.superseded_by != previous.superseded_by
                    }
                    RequirementStatus::Withdrawn => {
                        next.status != RequirementStatus::Withdrawn || next.superseded_by.is_some()
                    }
                }
        {
            return None;
        }
    }
    Some(PreparedSourceMaterialization {
        local_classifications,
        requirements,
        mappings,
    })
}

fn prepared_materialization_covers_manifest_gaps(
    dossier: &CompletionReviewDossier,
    gaps: &[ManifestGapInput],
    prepared: &PreparedSourceMaterialization,
) -> bool {
    if gaps.is_empty() {
        return false;
    }
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let existing = dossier
        .requirements
        .iter()
        .map(|requirement| ClassifiedRequirementRef {
            source_id: requirement.source_id.clone(),
            source_span: requirement.source_span.clone(),
        })
        .collect::<BTreeSet<_>>();
    let prepared_requirements = prepared
        .requirements
        .iter()
        .map(|requirement| {
            (
                ClassifiedRequirementRef {
                    source_id: requirement.source_id.clone(),
                    source_span: requirement.source_span.clone(),
                },
                requirement,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();

    gaps.iter().all(|gap| {
        let Some(source) = sources.get(gap.source_id.as_str()) else {
            return false;
        };
        let key = source_classification_cache_key(source);
        let Some(local) = prepared.local_classifications.get(&key) else {
            return false;
        };
        !gap.omitted_spans.is_empty()
            && gap.omitted_spans.iter().all(|span| {
                let reference = ClassifiedRequirementRef {
                    source_id: source.source_id.clone(),
                    source_span: span.clone(),
                };
                let Some(exact_material) = material_for_span(source, span) else {
                    return false;
                };
                let Some(requirement) = prepared_requirements.get(&reference) else {
                    return false;
                };
                !existing.contains(&reference)
                    && seen.insert(reference)
                    && local.local_kind == SourceLocalClassificationKind::RequirementBearing
                    && local.requirement_spans.binary_search(span).is_ok()
                    && requirement.requirement_id == deterministic_requirement_id(source, span)
                    && requirement.source_content_hash == source.content_hash
                    && requirement.exact_material == exact_material
            })
    })
}

fn dossier_sources_are_current(
    ledger: &CompletionReviewLedgerV2,
    expected_sources: &[UserSourceRecord],
) -> bool {
    let mut current_sources = ledger
        .source_records
        .values()
        .filter(|source| source.completion_epoch == ledger.completion_epoch)
        .cloned()
        .collect::<Vec<_>>();
    current_sources.sort_by_key(|source| (source.source_ordinal, source.content_ordinal));
    current_sources == expected_sources
        && current_sources.iter().all(|source| {
            source.content_hash
                == user_source_content_hash(
                    source.source_kind,
                    &source.exact_material,
                    source.availability,
                )
        })
}

pub(crate) fn deterministic_requirement_id(source: &UserSourceRecord, span: &SourceSpan) -> String {
    format!(
        "REQ-{}",
        canonical_hash(
            REQUIREMENT_MANIFEST_CANONICAL_FORMAT,
            &serde_json::json!({
                "sourceId": source.source_id,
                "sourceContentHash": source.content_hash,
                "sourceSpan": span,
            }),
        )
    )
}

fn user_source_content_hash(
    kind: UserSourceKind,
    exact_material: &str,
    availability: UserSourceAvailability,
) -> String {
    canonical_hash(
        USER_SOURCE_LEDGER_CANONICAL_FORMAT,
        &serde_json::json!({
            "kind": kind,
            "material": exact_material,
            "availability": availability,
        }),
    )
}

fn deterministic_source_id(
    root_task_id: &str,
    completion_epoch: u64,
    message_id: &str,
    content_ordinal: u64,
    content_hash: &str,
) -> String {
    format!(
        "SRC-{}",
        canonical_hash(
            USER_SOURCE_LEDGER_CANONICAL_FORMAT,
            &serde_json::json!({
                "rootTaskId": root_task_id,
                "completionEpoch": completion_epoch,
                "messageId": message_id,
                "contentOrdinal": content_ordinal,
                "contentHash": content_hash,
            }),
        )
    )
}

fn source_mapping_revisions_for(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
) -> BTreeMap<String, SourceMappingRevision> {
    ledger
        .mapping_revisions
        .iter()
        .filter(|mapping| {
            mapping.completion_epoch == completion_epoch
                && mapping.manifest_revision == manifest_revision
        })
        .map(|mapping| (mapping.source_id.clone(), mapping.clone()))
        .collect()
}

fn source_mappings_for(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
) -> BTreeMap<String, SourceMapping> {
    source_mapping_revisions_for(ledger, completion_epoch, manifest_revision)
        .into_iter()
        .map(|(source_id, revision)| (source_id, revision.mapping))
        .collect()
}

fn user_source_ledger_snapshot_hash(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
    source_capture_failed: bool,
) -> String {
    let mut sources = ledger
        .source_records
        .values()
        .filter(|source| {
            source.completion_epoch == completion_epoch
                && source.introduced_manifest_revision <= manifest_revision
        })
        .cloned()
        .collect::<Vec<_>>();
    sources.sort_by_key(|source| (source.source_ordinal, source.content_ordinal));
    let mappings = source_mappings_for(ledger, completion_epoch, manifest_revision);
    canonical_hash(
        USER_SOURCE_LEDGER_CANONICAL_FORMAT,
        &serde_json::json!({
            "rootTaskId": ledger.root_task_id,
            "completionEpoch": completion_epoch,
            "manifestRevision": manifest_revision,
            "sources": sources,
            "mappings": mappings,
            "sourceCaptureFailed": source_capture_failed,
        }),
    )
}

fn active_source_mappings(ledger: &CompletionReviewLedgerV2) -> BTreeMap<String, SourceMapping> {
    source_mappings_for(ledger, ledger.completion_epoch, ledger.manifest_revision)
}

fn active_manifest(ledger: &CompletionReviewLedgerV2) -> Option<&RequirementManifestSnapshot> {
    ledger.manifest_snapshots.iter().rev().find(|manifest| {
        manifest.completion_epoch == ledger.completion_epoch
            && manifest.manifest_revision == ledger.manifest_revision
    })
}

fn requirement_manifest_hash(manifest_revision: u64, requirements: &[RequirementRecord]) -> String {
    let mut requirements = requirements.to_vec();
    requirements.sort_by(|left, right| left.requirement_id.cmp(&right.requirement_id));
    canonical_hash(
        REQUIREMENT_MANIFEST_CANONICAL_FORMAT,
        &serde_json::json!({
            "manifestRevision": manifest_revision,
            "requirements": requirements,
        }),
    )
}

fn completion_contract_hashes(
    document: &TaskEvidenceDocument,
    source_capture_failed: bool,
) -> Option<(u64, String, String)> {
    let ledger = document.completion_review_v2.as_ref()?;
    let requirements = active_manifest(ledger)
        .map(|manifest| manifest.requirements.clone())
        .unwrap_or_default();
    let source_hash = user_source_ledger_snapshot_hash(
        ledger,
        ledger.completion_epoch,
        ledger.manifest_revision,
        source_capture_failed,
    );
    let manifest_hash = requirement_manifest_hash(ledger.manifest_revision, &requirements);
    Some((ledger.manifest_revision, source_hash, manifest_hash))
}

fn canonical_hash(format_name: &str, value: &Value) -> String {
    let canonical = canonicalize_json_value(value.clone());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(format_name.as_bytes());
    bytes.push(b'\n');
    let Ok(serialized) = serde_json::to_vec(&canonical) else {
        unreachable!("canonical task evidence must serialize");
    };
    bytes.extend_from_slice(&serialized);
    format!("{:x}", Sha256::digest(bytes))
}

fn parse_desktop_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn current_desktop_activation_obligation(
    document: &TaskEvidenceDocument,
) -> Option<DesktopActivationObligation> {
    let mut requiring_plan_step_ids = document
        .plan
        .iter()
        .filter(|step| step.status != StepStatus::Skipped && step.requires_desktop_activation)
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();
    requiring_plan_step_ids.sort();
    requiring_plan_step_ids.dedup();
    if requiring_plan_step_ids.is_empty() {
        return None;
    }
    let completion = document.completion_review_v2.as_ref();
    let implementation_identity = canonical_hash(
        "KD4_DESKTOP_ACTIVATION_IMPLEMENTATION_IDENTITY_V1",
        &serde_json::json!({
            "threadId": document.thread_id,
            "evidenceEpoch": document.evidence_epoch,
            "hostMutationRevision": document.host_mutation_revision,
            "completionEpoch": completion.map(|ledger| ledger.completion_epoch),
            "manifestRevision": completion.map(|ledger| ledger.manifest_revision),
            "latestFileHashes": document.latest_file_hashes,
            "latestGeneratedArtifactHashes": document.latest_generated_artifact_hashes,
        }),
    );
    let activation_obligation_identity = canonical_hash(
        "KD4_DESKTOP_ACTIVATION_OBLIGATION_IDENTITY_V1",
        &serde_json::json!({
            "threadId": document.thread_id,
            "evidenceEpoch": document.evidence_epoch,
            "implementationIdentity": implementation_identity,
            "requiringPlanStepIds": requiring_plan_step_ids,
        }),
    );
    Some(DesktopActivationObligation {
        thread_id: document.thread_id.clone(),
        evidence_epoch: document.evidence_epoch,
        implementation_identity,
        activation_obligation_identity,
        requiring_plan_step_ids,
    })
}

fn desktop_activation_challenge_public(
    pending: &PendingDesktopActivationChallenge,
) -> DesktopActivationChallenge {
    DesktopActivationChallenge {
        challenge_id: pending.challenge_identity.clone(),
        thread_id: pending.thread_id.clone(),
        evidence_epoch: pending.evidence_epoch,
        implementation_identity: pending.implementation_identity_hash.clone(),
        activation_obligation_identity: pending.activation_obligation_identity.clone(),
        publisher_evidence_id: pending.publisher_evidence_id.clone(),
        expected_installed_executable_path: pending.expected_installed_executable_path.clone(),
        expected_installed_executable_sha256: pending.installed_executable_sha256.clone(),
        publish_id: pending.publish_identity.clone(),
        issued_at: pending.issued_at.clone(),
        expires_at: pending.expires_at.clone(),
    }
}

async fn verified_desktop_running_process(
    evidence: &AuthoritativeDesktopInstallEvidence,
) -> Result<DesktopRunningProcessObservation, DesktopActivationVerificationError> {
    let expected = tokio::fs::canonicalize(&evidence.expected_installed_executable_path)
        .await
        .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?;
    let running = tokio::fs::canonicalize(
        std::env::current_exe()
            .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?,
    )
    .await
    .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?;
    let sha256 = sha256_file(&expected)
        .await
        .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?;
    if !desktop_paths_match(&expected.to_string_lossy(), &running.to_string_lossy())
        || !sha256.eq_ignore_ascii_case(&evidence.installed_executable_sha256)
        || !sha256.eq_ignore_ascii_case(&evidence.publish_identity)
    {
        return Err(DesktopActivationVerificationError::RunningExecutableMismatch);
    }
    let process_id = std::process::id();
    Ok(DesktopRunningProcessObservation {
        process_id,
        process_identity: canonical_hash(
            "KD4_DESKTOP_RUNNING_PROCESS_IDENTITY_V1",
            &serde_json::json!({
                "processId": process_id,
                "path": normalize_path_for_identity(&running),
                "sha256": &sha256,
            }),
        ),
        executable_path: running.to_string_lossy().into_owned(),
        executable_sha256: sha256,
        observed_at: Utc::now().to_rfc3339(),
    })
}

async fn verify_pending_desktop_running_process(
    pending: &PendingDesktopActivationChallenge,
) -> Result<(), DesktopActivationVerificationError> {
    let expected = tokio::fs::canonicalize(&pending.expected_installed_executable_path)
        .await
        .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?;
    let running = tokio::fs::canonicalize(
        std::env::current_exe()
            .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?,
    )
    .await
    .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?;
    let sha256 = sha256_file(&expected)
        .await
        .map_err(|_| DesktopActivationVerificationError::RunningExecutableMismatch)?;
    if !desktop_paths_match(&expected.to_string_lossy(), &running.to_string_lossy())
        || !sha256.eq_ignore_ascii_case(&pending.installed_executable_sha256)
    {
        return Err(DesktopActivationVerificationError::RunningExecutableMismatch);
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn desktop_paths_match(expected: &str, observed: &str) -> bool {
    let expected = normalize_path_for_identity(Path::new(expected));
    let observed = normalize_path_for_identity(Path::new(observed));
    #[cfg(windows)]
    {
        expected.eq_ignore_ascii_case(&observed)
    }
    #[cfg(not(windows))]
    {
        expected == observed
    }
}

fn desktop_install_evidence_hash(
    evidence: &AuthoritativeDesktopInstallEvidence,
    authenticated_channel_identity: &str,
    authenticated_peer_identity: &str,
) -> String {
    canonical_hash(
        DESKTOP_INSTALL_EVIDENCE_CANONICAL_FORMAT,
        &serde_json::json!({
            "schemaVersion": evidence.schema_version,
            "trustedProducerVersion": evidence.trusted_producer_version,
            "publisherEvidenceId": evidence.publisher_evidence_id,
            "threadId": evidence.thread_id,
            "evidenceEpoch": evidence.evidence_epoch,
            "implementationIdentity": evidence.implementation_identity_hash,
            "activationObligationIdentity": evidence.activation_obligation_identity,
            "publishIdentity": evidence.publish_identity,
            "installGeneration": evidence.install_generation,
            "expectedInstalledExecutablePath": normalize_path_for_identity(Path::new(
                &evidence.expected_installed_executable_path,
            )),
            "installedExecutableSha256": evidence.installed_executable_sha256,
            "issuedAt": evidence.issued_at,
            "expiresAt": evidence.expires_at,
            "authenticatedHostChannel": authenticated_channel_identity,
            "authenticatedHostPeer": authenticated_peer_identity,
        }),
    )
}

fn desktop_activation_receipt_hash(receipt: &DesktopActivationReceipt) -> String {
    canonical_hash(
        DESKTOP_ACTIVATION_RECEIPT_CANONICAL_FORMAT,
        &serde_json::to_value(receipt).unwrap_or(Value::Null),
    )
}

fn validate_authoritative_desktop_install_evidence(
    evidence: &AuthoritativeDesktopInstallEvidence,
    authenticated_channel_identity: &str,
    authenticated_peer_identity: &str,
    current_obligation: &DesktopActivationObligation,
    now: DateTime<Utc>,
) -> Result<(), DesktopActivationVerificationError> {
    if evidence.schema_version != DESKTOP_INSTALL_EVIDENCE_SCHEMA_VERSION
        || !is_sha256_hex(&evidence.implementation_identity_hash)
        || evidence.trusted_producer_version != DESKTOP_INSTALL_EVIDENCE_SCHEMA_VERSION
        || evidence.publisher_evidence_id.trim().is_empty()
        || !is_sha256_hex(&evidence.publish_identity)
        || !Path::new(&evidence.expected_installed_executable_path).is_absolute()
        || !is_sha256_hex(&evidence.installed_executable_sha256)
        || !evidence
            .publish_identity
            .eq_ignore_ascii_case(&evidence.installed_executable_sha256)
        || authenticated_channel_identity.trim().is_empty()
        || authenticated_peer_identity.trim().is_empty()
        || authenticated_peer_identity != evidence.publisher_evidence_id
    {
        return Err(DesktopActivationVerificationError::InvalidAuthoritativeEvidence);
    }
    if evidence.thread_id != current_obligation.thread_id
        || evidence.evidence_epoch != current_obligation.evidence_epoch
        || evidence.implementation_identity_hash != current_obligation.implementation_identity
        || evidence.activation_obligation_identity
            != current_obligation.activation_obligation_identity
    {
        return Err(DesktopActivationVerificationError::ImplementationIdentityMismatch);
    }
    let issued_at = parse_desktop_timestamp(&evidence.issued_at)
        .ok_or(DesktopActivationVerificationError::InvalidAuthoritativeEvidence)?;
    if issued_at > now {
        return Err(DesktopActivationVerificationError::AuthoritativeEvidenceStale);
    }
    Ok(())
}

fn desktop_activation_receipt_is_complete(
    receipt: &DesktopActivationReceipt,
    runtime: &DesktopActivationRuntimeSnapshot,
    document: &TaskEvidenceDocument,
) -> bool {
    desktop_activation_receipt_is_complete_at(receipt, runtime, document, Utc::now())
}

fn desktop_activation_receipt_is_complete_at(
    receipt: &DesktopActivationReceipt,
    runtime: &DesktopActivationRuntimeSnapshot,
    document: &TaskEvidenceDocument,
    now: DateTime<Utc>,
) -> bool {
    let Some(obligation) = current_desktop_activation_obligation(document) else {
        return false;
    };
    let last_mutation_at = document
        .last_mutation_at
        .as_deref()
        .and_then(parse_desktop_timestamp);
    desktop_activation_receipt_matches_live_proof_at(
        receipt,
        runtime,
        &obligation,
        last_mutation_at,
        now,
    )
}

fn desktop_activation_receipt_matches_live_proof_at(
    receipt: &DesktopActivationReceipt,
    runtime: &DesktopActivationRuntimeSnapshot,
    obligation: &DesktopActivationObligation,
    last_mutation_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    let Some(live) = runtime.live_proof.as_ref() else {
        // Persisted and legacy receipts are audit evidence only. A process
        // restart deliberately drops this non-serializable live proof.
        return false;
    };
    let timestamps = [
        &receipt.publish_install_timestamp,
        &receipt.bootstrap_consumed_timestamp,
        &receipt.running_executable_observed_timestamp,
        &receipt.challenge_issued_timestamp,
        &receipt.observation_timestamp,
        &receipt.activation_timestamp,
    ]
    .map(|timestamp| parse_desktop_timestamp(timestamp));
    let [
        Some(installed_at),
        Some(bootstrap_at),
        Some(running_at),
        Some(issued_at),
        Some(observed_at),
        Some(recorded_at),
    ] = timestamps
    else {
        return false;
    };
    let post_mutation = last_mutation_at.is_none_or(|mutated_at| installed_at >= mutated_at);
    runtime.availability == DesktopInstallEvidenceAvailability::AuthenticatedHostBootstrap
        && receipt.trusted_producer_version == DESKTOP_INSTALL_EVIDENCE_SCHEMA_VERSION
        && receipt.thread_id == obligation.thread_id
        && receipt.epoch == live.evidence_epoch
        && receipt.epoch == obligation.evidence_epoch
        && receipt.implementation_identity_hash.as_deref()
            == Some(live.implementation_identity_hash.as_str())
        && receipt.implementation_identity_hash.as_deref()
            == Some(obligation.implementation_identity.as_str())
        && receipt.activation_obligation_identity == live.activation_obligation_identity
        && receipt.activation_obligation_identity == obligation.activation_obligation_identity
        && receipt.authoritative_install_evidence_hash == live.authoritative_install_evidence_hash
        && runtime.current_install_evidence_hash.as_deref()
            == Some(live.authoritative_install_evidence_hash.as_str())
        && receipt.publish_identity == live.publish_identity
        && receipt.install_generation == live.install_generation
        && receipt.authenticated_host_channel_identity == live.authenticated_host_channel_identity
        && receipt.running_process_id == live.running_process_id
        && receipt.running_process_identity == live.running_process_identity
        && receipt.running_process_id != 0
        && receipt.desktop_process_id != 0
        && !receipt.publisher_evidence_id.trim().is_empty()
        && !receipt.challenge_identity.trim().is_empty()
        && !receipt
            .initialization_observation_identity
            .trim()
            .is_empty()
        && Path::new(&receipt.expected_installed_executable_path).is_absolute()
        && Path::new(&receipt.observed_running_executable_path).is_absolute()
        && Path::new(&receipt.desktop_executable_path).is_absolute()
        && desktop_paths_match(
            &receipt.expected_installed_executable_path,
            &receipt.observed_running_executable_path,
        )
        && is_sha256_hex(&receipt.installed_executable_sha256)
        && is_sha256_hex(&receipt.observed_running_executable_sha256)
        && is_sha256_hex(&receipt.publish_identity)
        && receipt
            .publish_identity
            .eq_ignore_ascii_case(&receipt.installed_executable_sha256)
        && receipt
            .installed_executable_sha256
            .eq_ignore_ascii_case(&receipt.observed_running_executable_sha256)
        && post_mutation
        && installed_at <= bootstrap_at
        && bootstrap_at <= running_at
        && running_at <= issued_at
        && issued_at <= observed_at
        && observed_at <= recorded_at
        && recorded_at <= now
        && parse_desktop_timestamp(&receipt.challenge_expires_at)
            .is_some_and(|expires_at| observed_at <= expires_at && now <= expires_at)
        && parse_desktop_timestamp(&live.fresh_until).is_some_and(|fresh_until| now <= fresh_until)
        && desktop_activation_receipt_hash(receipt) == live.receipt_hash
}

pub(crate) async fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; FILE_HASH_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalize_path_for_identity(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_repair_path(
    path: &str,
    allow_wildcards: bool,
) -> Result<String, RereviewFallbackReason> {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.as_bytes().get(1) == Some(&b':')
    {
        return Err(RereviewFallbackReason::InvalidPath);
    }
    let mut segments = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(RereviewFallbackReason::InvalidPath),
            _ if !allow_wildcards && segment.contains('*') => {
                return Err(RereviewFallbackReason::InvalidPath);
            }
            _ => segments.push(segment),
        }
    }
    if segments.is_empty() {
        return Err(RereviewFallbackReason::InvalidPath);
    }
    let canonical = segments.join("/");
    if cfg!(windows) {
        if !canonical.is_ascii() {
            return Err(RereviewFallbackReason::AmbiguousWindowsCase);
        }
        Ok(canonical.to_ascii_lowercase())
    } else {
        Ok(canonical)
    }
}

fn generated_pattern_is_supported(pattern: &str, grammar_version: u32) -> bool {
    if grammar_version != REPAIR_PATH_GRAMMAR_VERSION {
        return false;
    }
    let Ok(pattern) = canonical_repair_path(pattern, true) else {
        return false;
    };
    let mut recursive_wildcards = 0;
    for segment in pattern.split('/') {
        match segment {
            "*" => {}
            "**" => recursive_wildcards += 1,
            literal if !literal.contains('*') && !literal.contains('?') => {}
            _ => return false,
        }
    }
    recursive_wildcards <= 1
}

fn generated_pattern_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", tail)) => (0..=path.len().min(REPAIR_RECURSIVE_WILDCARD_MAX_SEGMENTS))
            .any(|consumed| generated_pattern_matches(tail, &path[consumed..])),
        Some((&"*", tail)) => path
            .split_first()
            .is_some_and(|(_, rest)| generated_pattern_matches(tail, rest)),
        Some((literal, tail)) => path
            .split_first()
            .is_some_and(|(value, rest)| value == literal && generated_pattern_matches(tail, rest)),
    }
}

fn repair_scope_matches(
    scope: &RepairPathScope,
    path: &str,
) -> Result<bool, RereviewFallbackReason> {
    let path = canonical_repair_path(path, false)?;
    match scope {
        RepairPathScope::ExactFile { path: expected } => {
            Ok(canonical_repair_path(expected, false)? == path)
        }
        RepairPathScope::DirectoryPrefix { path: prefix } => {
            let prefix = canonical_repair_path(prefix, false)?;
            Ok(path == prefix || path.starts_with(&format!("{prefix}/")))
        }
        RepairPathScope::GeneratedPattern {
            grammar_version,
            pattern,
        } => {
            if !generated_pattern_is_supported(pattern, *grammar_version) {
                return Err(RereviewFallbackReason::UnsupportedPathGrammar);
            }
            let pattern = canonical_repair_path(pattern, true)?;
            Ok(generated_pattern_matches(
                &pattern.split('/').collect::<Vec<_>>(),
                &path.split('/').collect::<Vec<_>>(),
            ))
        }
    }
}

fn path_resolves_within_repository(
    repository_root: &str,
    path: &str,
) -> Result<(), RereviewFallbackReason> {
    let canonical = canonical_repair_path(path, false)?;
    let root = std::fs::canonicalize(repository_root)
        .map_err(|_| RereviewFallbackReason::SymlinkEscape)?;
    let candidate = root.join(canonical.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or(RereviewFallbackReason::SymlinkEscape)?;
    }
    let resolved =
        std::fs::canonicalize(existing).map_err(|_| RereviewFallbackReason::SymlinkEscape)?;
    if !resolved.starts_with(&root) {
        return Err(RereviewFallbackReason::SymlinkEscape);
    }
    Ok(())
}

fn plan_structure_hash(plan: &[EvidencePlanStep]) -> String {
    let structural_steps = plan
        .iter()
        .map(|step| {
            serde_json::json!({
                "id": step.id,
                "revision": step.revision,
                "step": step.step,
                "sourceOwner": step.source_owner,
                "implementationSurfaces": step.implementation_surfaces,
                "mutationObligations": step.mutation_obligations.iter().map(|obligation| serde_json::json!({
                    "id": obligation.id,
                    "description": obligation.description,
                    "paths": obligation.paths,
                })).collect::<Vec<_>>(),
                "validationDisposition": step.validation_disposition,
                "validationRoute": step.validation_route,
                "externalValidationRoute": step.external_validation_route,
                "dependsOn": step.depends_on,
                "acceptanceCriteria": step.acceptance_criteria,
                "runtimePaths": step.runtime_paths,
                "generatedArtifacts": step.generated_artifacts,
                "risks": step.risks,
                "requiresDesktopActivation": step.requires_desktop_activation,
                "editPaths": step.edit_paths,
            })
        })
        .collect::<Vec<_>>();
    canonical_hash(
        "KD4_PLAN_STRUCTURE_CANONICAL_V1",
        &serde_json::json!({ "steps": structural_steps }),
    )
}

fn path_scope_identifier(scope: &RepairPathScope) -> String {
    match scope {
        RepairPathScope::ExactFile { path } => format!("exact:{path}"),
        RepairPathScope::DirectoryPrefix { path } => format!("prefix:{path}"),
        RepairPathScope::GeneratedPattern {
            grammar_version,
            pattern,
        } => format!("generated:v{grammar_version}:{pattern}"),
    }
}

fn declared_repair_scopes(
    plan: &[EvidencePlanStep],
    paths: impl IntoIterator<Item = String>,
) -> (Vec<RepairPathScope>, Vec<RereviewFallbackReason>) {
    let mut scopes = Vec::new();
    let mut errors = BTreeSet::new();
    let candidates = paths.into_iter().chain(plan.iter().flat_map(|step| {
        step.edit_paths
            .iter()
            .chain(step.runtime_paths.iter())
            .chain(step.generated_artifacts.iter())
            .cloned()
    }));
    for candidate in candidates {
        let scope = if candidate.contains('*') {
            match canonical_repair_path(&candidate, true) {
                Ok(pattern)
                    if generated_pattern_is_supported(&pattern, REPAIR_PATH_GRAMMAR_VERSION) =>
                {
                    RepairPathScope::GeneratedPattern {
                        grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
                        pattern,
                    }
                }
                Ok(_) => {
                    errors.insert(RereviewFallbackReason::UnsupportedPathGrammar);
                    continue;
                }
                Err(reason) => {
                    errors.insert(reason);
                    continue;
                }
            }
        } else if candidate.ends_with('/') || candidate.ends_with('\\') {
            match canonical_repair_path(&candidate, false) {
                Ok(path) => RepairPathScope::DirectoryPrefix { path },
                Err(reason) => {
                    errors.insert(reason);
                    continue;
                }
            }
        } else {
            match canonical_repair_path(&candidate, false) {
                Ok(path) => RepairPathScope::ExactFile { path },
                Err(reason) => {
                    errors.insert(reason);
                    continue;
                }
            }
        };
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    scopes.sort_by_key(path_scope_identifier);
    (scopes, errors.into_iter().collect())
}

fn receipt_sequence(id: &str, prefix: &str) -> Option<u64> {
    id.strip_prefix(prefix)?.parse().ok()
}

fn current_repair_snapshot(
    document: &TaskEvidenceDocument,
    typed_mutation_identities: &[String],
) -> CurrentRepairSnapshot {
    let mut containment_errors = BTreeSet::new();
    let mut path_states = Vec::new();
    for snapshot in document
        .latest_file_hashes
        .values()
        .chain(document.latest_generated_artifact_hashes.values())
    {
        match canonical_repair_path(&snapshot.path, false) {
            Ok(path) => path_states.push(RepairPathState {
                path,
                exists: snapshot.exists,
                content_hash: snapshot.sha1.clone(),
            }),
            Err(reason) => {
                containment_errors.insert(reason);
            }
        }
        if snapshot.read_error.is_some() {
            containment_errors.insert(RereviewFallbackReason::UnrepresentableEvidenceChange);
        }
    }
    path_states.sort_by(|left, right| left.path.cmp(&right.path));
    path_states.dedup_by(|left, right| left.path == right.path);

    let (declared_path_scopes, scope_errors) = declared_repair_scopes(
        &document.plan,
        path_states.iter().map(|state| state.path.clone()),
    );
    containment_errors.extend(scope_errors);
    let implementation_surfaces = declared_path_scopes
        .iter()
        .map(|scope| StructuredContractSurface {
            kind: "path_scope".to_string(),
            owner: "task".to_string(),
            identifier: path_scope_identifier(scope),
        })
        .collect::<Vec<_>>();

    let mut command_receipts = document
        .command_receipts
        .iter()
        .filter_map(|receipt| {
            let sequence = receipt_sequence(&receipt.id, "command-")?;
            Some(RepairCommandDelta {
                sequence,
                receipt_id: receipt.id.clone(),
                command: receipt.command.join(" "),
                cwd: normalize_path_for_identity(Path::new(&receipt.cwd)),
                exit_code: Some(receipt.exit_code),
                timed_out: receipt.timed_out,
                implementation_identity: receipt.implementation_identity_hash.clone(),
            })
        })
        .collect::<Vec<_>>();
    if command_receipts.len() != document.command_receipts.len() {
        containment_errors.insert(RereviewFallbackReason::CommandLineageChanged);
    }
    command_receipts.sort_by_key(|receipt| receipt.sequence);

    let mut default_child_mutation_identities = document
        .completion_review_v2
        .as_ref()
        .into_iter()
        .flat_map(|ledger| ledger.attributed_workspace_events.iter())
        .map(|event| {
            serde_json::to_string(&serde_json::json!({
                "workspaceId": event.workspace_id,
                "epoch": event.epoch,
                "actorId": event.actor_id,
                "paths": event.paths,
            }))
            .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    default_child_mutation_identities.sort();
    default_child_mutation_identities.dedup();

    let mut typed_mutation_identities = typed_mutation_identities.to_vec();
    typed_mutation_identities.sort();
    typed_mutation_identities.dedup();
    let mut external_evidence_ids = document
        .external_evidence
        .iter()
        .filter(|receipt| receipt.task_epoch == document.evidence_epoch)
        .map(|receipt| receipt.id.clone())
        .collect::<Vec<_>>();
    external_evidence_ids.sort();
    external_evidence_ids.dedup();

    CurrentRepairSnapshot {
        repository_root: document.start.repository_root.clone(),
        path_states,
        command_receipts,
        plan_structure_hash: plan_structure_hash(&document.plan),
        declared_path_scopes,
        implementation_surfaces,
        default_child_mutation_identities,
        typed_mutation_identities,
        external_evidence_ids,
        containment_errors: containment_errors.into_iter().collect(),
    }
}

pub(crate) fn build_repair_baseline(
    dossier: &CompletionReviewDossier,
    findings: &[CompletionReviewFindingReceipt],
) -> Result<RepairBaseline, RereviewFallbackReason> {
    let active_requirement_ids = dossier
        .requirements
        .iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .map(|requirement| requirement.requirement_id.clone())
        .collect::<BTreeSet<_>>();
    let affected_requirement_ids =
        derive_affected_requirement_ids(&active_requirement_ids, findings)?;
    if !dossier
        .current_repair_snapshot
        .containment_errors
        .is_empty()
    {
        return Err(dossier.current_repair_snapshot.containment_errors[0].clone());
    }
    let command_bindings = dossier
        .current_repair_snapshot
        .command_receipts
        .iter()
        .map(|receipt| BaselineCommandBinding {
            sequence: receipt.sequence,
            receipt_id: receipt.receipt_id.clone(),
            implementation_identity: receipt.implementation_identity.clone(),
        })
        .collect::<Vec<_>>();
    let command_sequence_high_water_mark = command_bindings
        .iter()
        .map(|binding| binding.sequence)
        .max()
        .unwrap_or(0);
    Ok(RepairBaseline {
        path_states: dossier.current_repair_snapshot.path_states.clone(),
        command_sequence_high_water_mark,
        command_bindings,
        implementation_surfaces: dossier
            .current_repair_snapshot
            .implementation_surfaces
            .clone(),
        repair_scope: RepairScope {
            path_grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
            paths: dossier.current_repair_snapshot.declared_path_scopes.clone(),
            surfaces: dossier
                .current_repair_snapshot
                .implementation_surfaces
                .clone(),
            affected_requirement_ids,
        },
        source_ledger_hash: dossier.user_source_ledger_hash.clone(),
        requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
        plan_structure_hash: dossier.current_repair_snapshot.plan_structure_hash.clone(),
        default_child_mutation_identities: dossier
            .current_repair_snapshot
            .default_child_mutation_identities
            .clone(),
        typed_mutation_identities: dossier
            .current_repair_snapshot
            .typed_mutation_identities
            .clone(),
        external_evidence_ids: dossier
            .current_repair_snapshot
            .external_evidence_ids
            .clone(),
    })
}

fn derive_affected_requirement_ids(
    active_requirement_ids: &BTreeSet<String>,
    findings: &[CompletionReviewFindingReceipt],
) -> Result<Vec<String>, RereviewFallbackReason> {
    if findings.is_empty() {
        return Ok(active_requirement_ids.iter().cloned().collect());
    }
    let referenced = findings
        .iter()
        .flat_map(|finding| finding.requirement_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    if referenced.is_empty() || !referenced.is_subset(active_requirement_ids) {
        return Err(RereviewFallbackReason::RequirementManifestChanged);
    }
    Ok(referenced.into_iter().collect())
}

fn mutation_paths_are_attributable(identity: &str, scopes: &[RepairPathScope]) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(identity) else {
        return false;
    };
    let Some(paths) = value.get("paths").and_then(Value::as_array) else {
        return false;
    };
    !paths.is_empty()
        && paths.iter().all(|path| {
            path.as_str().is_some_and(|path| {
                scopes
                    .iter()
                    .any(|scope| repair_scope_matches(scope, path).unwrap_or(false))
            })
        })
}

// Keep the baseline and current identity hashes explicit at this fail-closed boundary.
#[allow(clippy::too_many_arguments)]
fn build_rereview_input(
    baseline: Option<&RepairBaseline>,
    persisted_baseline_hash: Option<&str>,
    repair_instruction_hash: Option<&str>,
    original_findings: &[CompletionReviewFindingReceipt],
    current: &CurrentRepairSnapshot,
    candidate_implementation_identity: &str,
    source_ledger_hash: &str,
    requirement_manifest_hash: &str,
) -> Option<RereviewInput> {
    let repair_instruction_hash = repair_instruction_hash?.to_string();
    let mut reasons = current
        .containment_errors
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let Some(baseline) = baseline else {
        reasons.insert(RereviewFallbackReason::MissingBaseline);
        return Some(RereviewInput {
            input_mode: RereviewInputMode::FullFallback,
            baseline_hash: persisted_baseline_hash.map(str::to_string),
            delta_hash: None,
            fallback_reasons: reasons.into_iter().collect(),
            repair_instruction_hash,
            candidate_implementation_identity: candidate_implementation_identity.to_string(),
            delta: None,
        });
    };
    let computed_baseline_hash = repair_baseline_hash(baseline);
    if persisted_baseline_hash != Some(computed_baseline_hash.as_str()) {
        reasons.insert(RereviewFallbackReason::InvalidBaselineHash);
    }
    if baseline.repair_scope.path_grammar_version != REPAIR_PATH_GRAMMAR_VERSION {
        reasons.insert(RereviewFallbackReason::UnsupportedPathGrammar);
    }
    if baseline.source_ledger_hash != source_ledger_hash {
        reasons.insert(RereviewFallbackReason::SourceIdentityChanged);
    }
    if baseline.requirement_manifest_hash != requirement_manifest_hash {
        reasons.insert(RereviewFallbackReason::RequirementManifestChanged);
    }
    if baseline.plan_structure_hash != current.plan_structure_hash {
        reasons.insert(RereviewFallbackReason::PlanStructureChanged);
    }

    let before_paths = baseline
        .path_states
        .iter()
        .map(|state| (state.path.as_str(), state))
        .collect::<BTreeMap<_, _>>();
    let after_paths = current
        .path_states
        .iter()
        .map(|state| (state.path.as_str(), state))
        .collect::<BTreeMap<_, _>>();
    let all_paths = before_paths
        .keys()
        .chain(after_paths.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut path_changes = Vec::new();
    for path in all_paths {
        let before = before_paths.get(path).copied();
        let after = after_paths.get(path).copied();
        if before == after {
            continue;
        }
        match baseline
            .repair_scope
            .paths
            .iter()
            .map(|scope| repair_scope_matches(scope, path))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(matches) if matches.iter().any(|matches| *matches) => {}
            Ok(_) => {
                reasons.insert(RereviewFallbackReason::PathOutsideScope);
            }
            Err(reason) => {
                reasons.insert(reason);
            }
        }
        if let Err(reason) = path_resolves_within_repository(&current.repository_root, path) {
            reasons.insert(reason);
        }
        let before_exists = before.is_some_and(|state| state.exists);
        let after_exists = after.is_some_and(|state| state.exists);
        if !before_exists && !after_exists {
            continue;
        }
        let before_hash = before.and_then(|state| state.content_hash.clone());
        let after_hash = after.and_then(|state| state.content_hash.clone());
        let change = match (before_exists, after_exists) {
            (false, true) => RepairPathChangeKind::Added,
            (true, false) => RepairPathChangeKind::Removed,
            (true, true) => RepairPathChangeKind::Modified,
            (false, false) => unreachable!(),
        };
        path_changes.push(RepairPathChange {
            path: path.to_string(),
            change,
            before_exists,
            before_hash,
            after_exists,
            after_hash,
        });
    }

    let current_commands = current
        .command_receipts
        .iter()
        .map(|receipt| (receipt.sequence, receipt))
        .collect::<BTreeMap<_, _>>();
    let mut invalidated_command_receipts = Vec::new();
    for binding in &baseline.command_bindings {
        let Some(current_receipt) = current_commands.get(&binding.sequence) else {
            reasons.insert(RereviewFallbackReason::CommandLineageChanged);
            continue;
        };
        if current_receipt.receipt_id != binding.receipt_id {
            reasons.insert(RereviewFallbackReason::CommandLineageChanged);
        }
        if binding.implementation_identity.as_deref() != Some(candidate_implementation_identity) {
            invalidated_command_receipts.push(InvalidatedCommandReceipt {
                sequence: binding.sequence,
                receipt_id: binding.receipt_id.clone(),
                reason: "implementation_identity_binding_mismatch".to_string(),
            });
        }
    }
    let mut new_command_receipts = current
        .command_receipts
        .iter()
        .filter(|receipt| receipt.sequence > baseline.command_sequence_high_water_mark)
        .cloned()
        .collect::<Vec<_>>();
    new_command_receipts.sort_by_key(|receipt| receipt.sequence);

    let baseline_children = baseline
        .default_child_mutation_identities
        .iter()
        .collect::<BTreeSet<_>>();
    if current
        .default_child_mutation_identities
        .iter()
        .filter(|identity| !baseline_children.contains(identity))
        .any(|identity| !mutation_paths_are_attributable(identity, &baseline.repair_scope.paths))
    {
        reasons.insert(RereviewFallbackReason::UnattributedMutation);
    }
    let baseline_typed = baseline
        .typed_mutation_identities
        .iter()
        .collect::<BTreeSet<_>>();
    if current
        .typed_mutation_identities
        .iter()
        .filter(|identity| !baseline_typed.contains(identity))
        .any(|identity| !mutation_paths_are_attributable(identity, &baseline.repair_scope.paths))
    {
        reasons.insert(RereviewFallbackReason::UnattributedMutation);
    }
    if baseline.external_evidence_ids != current.external_evidence_ids {
        reasons.insert(RereviewFallbackReason::UnrepresentableEvidenceChange);
    }

    let permitted_surfaces = baseline
        .repair_scope
        .surfaces
        .iter()
        .collect::<BTreeSet<_>>();
    let baseline_surfaces = baseline
        .implementation_surfaces
        .iter()
        .collect::<BTreeSet<_>>();
    let mut newly_realized_surfaces = current
        .implementation_surfaces
        .iter()
        .filter(|surface| !baseline_surfaces.contains(surface))
        .cloned()
        .collect::<Vec<_>>();
    newly_realized_surfaces.sort();
    newly_realized_surfaces.dedup();
    if newly_realized_surfaces
        .iter()
        .any(|surface| !permitted_surfaces.contains(surface))
    {
        reasons.insert(RereviewFallbackReason::ContractSurfaceOutsideScope);
    }

    if !reasons.is_empty() {
        return Some(RereviewInput {
            input_mode: RereviewInputMode::FullFallback,
            baseline_hash: Some(computed_baseline_hash),
            delta_hash: None,
            fallback_reasons: reasons.into_iter().collect(),
            repair_instruction_hash,
            candidate_implementation_identity: candidate_implementation_identity.to_string(),
            delta: None,
        });
    }
    let delta = RepairDelta {
        original_findings: original_findings.to_vec(),
        required_disposition_finding_ids: original_findings
            .iter()
            .map(|finding| finding.finding_id.clone())
            .collect(),
        repair_instruction_hash: repair_instruction_hash.clone(),
        baseline_hash: computed_baseline_hash.clone(),
        candidate_implementation_identity: candidate_implementation_identity.to_string(),
        path_changes,
        new_command_receipts,
        invalidated_command_receipts,
        affected_requirement_ids: baseline.repair_scope.affected_requirement_ids.clone(),
        newly_realized_surfaces,
    };
    Some(RereviewInput {
        input_mode: RereviewInputMode::Delta,
        baseline_hash: Some(computed_baseline_hash),
        delta_hash: Some(repair_delta_hash(&delta)),
        fallback_reasons: Vec::new(),
        repair_instruction_hash,
        candidate_implementation_identity: candidate_implementation_identity.to_string(),
        delta: Some(delta),
    })
}

pub(crate) fn repair_baseline_hash(baseline: &RepairBaseline) -> String {
    canonical_hash(
        REPAIR_BASELINE_CANONICAL_FORMAT,
        &serde_json::to_value(baseline).unwrap_or(Value::Null),
    )
}

fn bind_initial_repair_baseline_metadata(
    baseline: Result<RepairBaseline, RereviewFallbackReason>,
    instruction: Option<&str>,
) -> Option<(RepairBaseline, String)> {
    let baseline = baseline.ok()?;
    let baseline_hash = repair_baseline_hash(&baseline);
    instruction
        .is_some_and(|instruction| {
            repair_instruction_matches_baseline(instruction, &baseline, &baseline_hash)
        })
        .then_some((baseline, baseline_hash))
}

pub(crate) fn repair_delta_hash(delta: &RepairDelta) -> String {
    canonical_hash(
        REPAIR_DELTA_CANONICAL_FORMAT,
        &serde_json::to_value(delta).unwrap_or(Value::Null),
    )
}

fn rereview_audit_hash(input: &RereviewInput) -> String {
    canonical_hash(
        REREVIEW_AUDIT_CANONICAL_FORMAT,
        &serde_json::to_value(input).unwrap_or(Value::Null),
    )
}

fn repair_instruction_matches_baseline(
    instruction: &str,
    baseline: &RepairBaseline,
    baseline_hash: &str,
) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(instruction) else {
        return false;
    };
    value.get("repair_baseline_hash").and_then(Value::as_str) == Some(baseline_hash)
        && value.get("declared_repair_scope")
            == serde_json::to_value(&baseline.repair_scope).ok().as_ref()
}

fn validate_rereview_input(input: &RereviewInput, dossier: &CompletionReviewDossier) -> bool {
    if input.repair_instruction_hash
        != dossier
            .initial_repair_instruction_hash
            .as_deref()
            .unwrap_or_default()
        || input.candidate_implementation_identity != dossier.implementation_identity_hash
    {
        return false;
    }
    match input.input_mode {
        RereviewInputMode::Delta => {
            let (Some(baseline), Some(baseline_hash), Some(delta), Some(delta_hash)) = (
                dossier.initial_repair_baseline.as_ref(),
                input.baseline_hash.as_deref(),
                input.delta.as_ref(),
                input.delta_hash.as_deref(),
            ) else {
                return false;
            };
            input.fallback_reasons.is_empty()
                && repair_baseline_hash(baseline) == baseline_hash
                && repair_delta_hash(delta) == delta_hash
                && delta.baseline_hash == baseline_hash
                && delta.repair_instruction_hash == input.repair_instruction_hash
                && delta.candidate_implementation_identity
                    == input.candidate_implementation_identity
                && validate_repair_delta_contents(delta, baseline, &dossier.original_findings)
                    .is_ok()
        }
        RereviewInputMode::FullFallback => {
            !input.fallback_reasons.is_empty()
                && input
                    .fallback_reasons
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                && input.delta_hash.is_none()
                && input.delta.is_none()
                && input.baseline_hash.as_deref() == dossier.initial_repair_baseline_hash.as_deref()
        }
    }
}

async fn persist_document_with_permit(
    path: PathBuf,
    document: TaskEvidenceDocument,
    permit: tokio::sync::OwnedSemaphorePermit,
    last_persisted_revision: Arc<AtomicU64>,
    test_control: Option<PersistenceTestControl>,
) -> (PersistOutcome, Option<tokio::sync::OwnedSemaphorePermit>) {
    let bytes = match serde_json::to_vec_pretty(&document) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("failed to serialize KD4 task evidence: {err}");
            return (PersistOutcome::Failed, Some(permit));
        }
    };
    match tokio::task::spawn_blocking(move || {
        if let Some(control) = test_control.as_ref()
            && let Some((started, release)) = control
                .before_next_write
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            started.wait();
            release.wait();
        }
        let last_revision = last_persisted_revision.load(Ordering::Acquire);
        let force_superseded = test_control.as_ref().is_some_and(|control| {
            control
                .supersede_writes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        });
        let outcome = if force_superseded || last_revision > document.revision {
            PersistOutcome::Superseded
        } else if last_revision == document.revision && last_revision != 0 {
            PersistOutcome::Persisted
        } else {
            let write_result = if test_control
                .as_ref()
                .is_some_and(|control| control.fail_writes.load(Ordering::Acquire))
            {
                Err(io::Error::other(
                    "injected task-evidence persistence failure",
                ))
            } else {
                atomic_write_evidence(&path, &bytes)
            };
            match write_result {
                Ok(()) => {
                    last_persisted_revision.store(document.revision, Ordering::Release);
                    PersistOutcome::Persisted
                }
                Err(err) => {
                    warn!("failed to persist KD4 task evidence: {err}");
                    PersistOutcome::Failed
                }
            }
        };
        (outcome, permit)
    })
    .await
    {
        Ok((outcome, permit)) => (outcome, Some(permit)),
        Err(err) => {
            warn!("KD4 task-evidence persistence task failed: {err}");
            (PersistOutcome::Failed, None)
        }
    }
}

async fn delete_external_artifact_owned(
    codex_home: Option<&Path>,
    thread_id: Option<&str>,
    artifact_id: Option<&str>,
) {
    let (Some(artifact_id), Some(codex_home), Some(thread_id)) =
        (artifact_id, codex_home, thread_id)
    else {
        return;
    };
    if let Err(err) = crate::tools::command_output_artifact::delete_evidence_artifact(
        codex_home,
        thread_id,
        artifact_id,
    )
    .await
    {
        warn!("failed to delete external evidence artifact {artifact_id}: {err}");
    }
}

struct ExternalEvidenceMetadata {
    producer: String,
    producer_schema_version: u32,
    provider_snapshot: Option<String>,
    payload_completeness: EvidenceCompleteness,
    truncated: bool,
    approximate: bool,
    limitations: Vec<String>,
}

fn extract_external_evidence_metadata(
    result: &CallToolResult,
) -> Result<Option<ExternalEvidenceMetadata>, &'static str> {
    let Some(structured) = result.structured_content.as_ref() else {
        return Ok(None);
    };
    let Some(evidence_meta) = structured.get("evidenceMeta") else {
        return Ok(None);
    };
    let Some(meta) = evidence_meta.as_object() else {
        return Err("MCP evidenceMeta is malformed and was ignored");
    };
    let Some(schema_version) = meta.get("schemaVersion").and_then(Value::as_u64) else {
        return Err("MCP evidenceMeta schemaVersion is malformed and was ignored");
    };
    if schema_version != 1 {
        return Err("MCP evidenceMeta schemaVersion is unsupported and was ignored");
    }
    let Some(producer) = meta.get("producer").and_then(Value::as_str) else {
        return Err("MCP evidenceMeta producer is malformed and was ignored");
    };
    if producer.trim().is_empty() {
        return Err("MCP evidenceMeta producer is malformed and was ignored");
    }
    let Some(evidence_bearing) = meta.get("evidenceBearing").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta evidenceBearing is malformed and was ignored");
    };
    if !evidence_bearing {
        return Ok(None);
    }
    if meta
        .get("operation")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("MCP evidenceMeta operation is malformed and was ignored");
    }
    let payload_completeness = match meta.get("payloadCompleteness").and_then(Value::as_str) {
        Some("complete") => EvidenceCompleteness::Complete,
        Some("partial") => EvidenceCompleteness::Partial,
        Some("unknown") => EvidenceCompleteness::Unknown,
        _ => {
            return Err("MCP evidenceMeta payloadCompleteness is malformed and was ignored");
        }
    };
    let Some(truncated) = meta.get("truncated").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta truncated flag is malformed and was ignored");
    };
    if payload_completeness == EvidenceCompleteness::Complete && truncated {
        return Err("MCP evidenceMeta complete payload cannot be truncated and was ignored");
    }
    let Some(approximate) = meta.get("approximate").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta approximate flag is malformed and was ignored");
    };
    let Some(limitations) = meta.get("limitations").and_then(Value::as_array) else {
        return Err("MCP evidenceMeta limitations are malformed and were ignored");
    };
    let Some(limitations) = limitations
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
    else {
        return Err("MCP evidenceMeta limitations are malformed and were ignored");
    };
    let provider_snapshot = match meta.get("snapshot") {
        // Keep ingress tolerant for providers deployed before snapshot became
        // explicitly required by the producer contract.
        None | Some(Value::Null) => None,
        Some(Value::String(snapshot)) => Some(snapshot.clone()),
        Some(_) => return Err("MCP evidenceMeta snapshot is malformed and was ignored"),
    };
    Ok(Some(ExternalEvidenceMetadata {
        producer: producer.to_string(),
        producer_schema_version: schema_version as u32,
        provider_snapshot,
        payload_completeness,
        truncated,
        approximate,
        limitations,
    }))
}

fn canonical_mcp_result_payload(result: &CallToolResult) -> Value {
    canonicalize_json_value(serde_json::json!({
        "content": result.content,
        "structuredContent": result.structured_content,
        "isError": result.is_error,
    }))
}

fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json_value(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => {
            Value::Array(values.into_iter().map(canonicalize_json_value).collect())
        }
        other => other,
    }
}

const fn evidence_completeness_name(completeness: EvidenceCompleteness) -> &'static str {
    match completeness {
        EvidenceCompleteness::Complete => "complete",
        EvidenceCompleteness::Partial => "partial",
        EvidenceCompleteness::Unknown => "unknown",
    }
}

fn encode_external_evidence_artifact(canonical_bytes: &[u8]) -> Option<Vec<u8>> {
    let canonical = std::str::from_utf8(canonical_bytes).ok()?;
    let mut encoded = Vec::with_capacity(canonical_bytes.len() + 256);
    encoded.extend_from_slice(EXTERNAL_EVIDENCE_ARTIFACT_HEADER.as_bytes());
    let mut start = 0;
    while start < canonical.len() {
        let mut end = (start + EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES).min(canonical.len());
        while !canonical.is_char_boundary(end) {
            end -= 1;
        }
        let line = Value::String(canonical[start..end].to_string()).to_string();
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
        start = end;
    }
    Some(encoded)
}

fn workspace_root_fingerprint(start: &TaskStartState) -> String {
    let identity = canonicalize_json_value(serde_json::json!({
        "repositoryRoot": start.repository_root,
        "repositoryUrl": start.repository_url,
    }));
    let serialized = identity.to_string();
    format!("{:x}", Sha256::digest(serialized.as_bytes()))
}

enum ExistingDocument {
    Missing,
    Loaded {
        document: Box<TaskEvidenceDocument>,
        legacy_completion_model: bool,
    },
    NewerSchema {
        schema_version: u64,
    },
    Rejected {
        kind: &'static str,
        reason: String,
    },
}

async fn load_existing_document(
    path: &Path,
    expected_thread_id: &str,
    expected_repository_root: &Path,
) -> ExistingDocument {
    load_existing_document_with_supported_version(
        path,
        expected_thread_id,
        expected_repository_root,
        TASK_EVIDENCE_SCHEMA_VERSION,
    )
    .await
}

async fn load_existing_document_with_supported_version(
    path: &Path,
    expected_thread_id: &str,
    expected_repository_root: &Path,
    max_supported_schema_version: u32,
) -> ExistingDocument {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ExistingDocument::Missing,
        Err(err) => {
            return ExistingDocument::Rejected {
                kind: "unreadable",
                reason: format!("could not read evidence: {err}"),
            };
        }
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(err) => {
            return ExistingDocument::Rejected {
                kind: "corrupt",
                reason: format!("invalid JSON: {err}"),
            };
        }
    };
    let schema_version = match value.get("schema_version").and_then(Value::as_u64) {
        Some(schema_version) => schema_version,
        None => {
            return ExistingDocument::Rejected {
                kind: "incompatible",
                reason: "missing numeric schema_version".to_string(),
            };
        }
    };
    if schema_version > u64::from(max_supported_schema_version) {
        return ExistingDocument::NewerSchema { schema_version };
    }
    if schema_version == 0 {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: format!("unsupported schema version {schema_version}"),
        };
    }
    let schema_version = schema_version as u32;
    if schema_version >= FROZEN_TASK_EVIDENCE_V6_SCHEMA_VERSION
        && !value
            .get("source_classification_cache")
            .is_some_and(Value::is_array)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: "V6+ requires an array source_classification_cache".to_string(),
        };
    }
    let legacy_completion_model = schema_version < TASK_EVIDENCE_COMPLETION_MODEL_VERSION
        || uses_retired_v3_completion_shape(schema_version, &value);
    let document = match serde_json::from_value::<TaskEvidenceDocument>(value) {
        Ok(document) => document,
        Err(err) => {
            return ExistingDocument::Rejected {
                kind: "corrupt",
                reason: format!("schema-valid JSON could not be decoded: {err}"),
            };
        }
    };
    if document.thread_id != expected_thread_id {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: "thread id does not match the requested task".to_string(),
        };
    }
    if !recorded_repository_root_matches(&document.start.repository_root, expected_repository_root)
    {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: "repository root does not match the requested checkout".to_string(),
        };
    }
    if matches!(
        schema_version,
        FROZEN_TASK_EVIDENCE_V5_SCHEMA_VERSION
            | FROZEN_TASK_EVIDENCE_V6_SCHEMA_VERSION
            | FROZEN_TASK_EVIDENCE_V7_SCHEMA_VERSION
            | FROZEN_TASK_EVIDENCE_V8_SCHEMA_VERSION
            | FROZEN_TASK_EVIDENCE_V9_SCHEMA_VERSION
            | FROZEN_TASK_EVIDENCE_V10_SCHEMA_VERSION
            | FROZEN_TASK_EVIDENCE_V11_SCHEMA_VERSION
            | TASK_EVIDENCE_SCHEMA_VERSION
    ) && let Err(reason) = validate_v5_completion_review(&document)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: format!("invalid V5 completion-review lineage: {reason}"),
        };
    }
    if schema_version >= FROZEN_TASK_EVIDENCE_V6_SCHEMA_VERSION
        && let Err(reason) = validate_v6_source_classification_state(&document)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: format!("invalid V6+ source-classification state: {reason}"),
        };
    }
    ExistingDocument::Loaded {
        document: Box::new(document),
        legacy_completion_model,
    }
}

fn validate_v6_source_classification_state(document: &TaskEvidenceDocument) -> Result<(), String> {
    if document.source_classification_cache
        != canonical_source_classification_cache(document.source_classification_cache.clone())
    {
        return Err("cache is not canonical, sorted, and unique".to_string());
    }
    let Some(ledger) = document.completion_review_v2.as_ref() else {
        return Ok(());
    };
    for revision in &ledger.mapping_revisions {
        match (
            revision.source_classification_contract_version.as_deref(),
            revision.relationship_resolver_contract_version.as_deref(),
        ) {
            (None, None) => {}
            (Some(source_version), Some(resolver_version))
                if !source_version.trim().is_empty() && !resolver_version.trim().is_empty() => {}
            _ => return Err("mapping revision contract versions are incomplete".to_string()),
        }
    }
    Ok(())
}

fn requirement_supersession_is_acyclic(requirements: &[RequirementRecord]) -> bool {
    let by_id = requirements
        .iter()
        .map(|requirement| (requirement.requirement_id.as_str(), requirement))
        .collect::<BTreeMap<_, _>>();
    requirements.iter().all(|requirement| {
        let mut visited = BTreeSet::new();
        let mut current = Some(requirement.requirement_id.as_str());
        while let Some(requirement_id) = current {
            if !visited.insert(requirement_id) {
                return false;
            }
            let Some(requirement) = by_id.get(requirement_id) else {
                return false;
            };
            current = requirement.superseded_by.as_deref();
        }
        true
    })
}

fn validate_v5_completion_review(document: &TaskEvidenceDocument) -> Result<(), String> {
    let ledger = document
        .completion_review_v2
        .as_ref()
        .ok_or_else(|| "missing completion_review_v2 ledger".to_string())?;
    if ledger.root_task_id != document.thread_id || ledger.completion_epoch == 0 {
        return Err("root task or completion epoch is inconsistent".to_string());
    }
    let mut source_ordinals = BTreeSet::new();
    for (source_id, source) in &ledger.source_records {
        let expected_content_hash = user_source_content_hash(
            source.source_kind,
            &source.exact_material,
            source.availability,
        );
        let expected_source_id = deterministic_source_id(
            &ledger.root_task_id,
            source.completion_epoch,
            &source.message_id,
            source.content_ordinal,
            &expected_content_hash,
        );
        if source_id != &source.source_id
            || source_id != &expected_source_id
            || source.content_hash != expected_content_hash
            || source.completion_epoch == 0
            || source.completion_epoch > ledger.completion_epoch
            || !source_ordinals.insert((source.completion_epoch, source.source_ordinal))
        {
            return Err(
                "source record identity, content hash, ordinal, or epoch is inconsistent"
                    .to_string(),
            );
        }
    }

    let mut mapping_keys = BTreeSet::new();
    for mapping in &ledger.mapping_revisions {
        let Some(source) = ledger.source_records.get(&mapping.source_id) else {
            return Err("source mapping references an unknown source".to_string());
        };
        if source.completion_epoch != mapping.completion_epoch
            || !mapping_keys.insert((
                mapping.completion_epoch,
                mapping.manifest_revision,
                mapping.source_id.clone(),
            ))
        {
            return Err("source mapping revision is duplicated or cross-epoch".to_string());
        }
        if matches!(
            &mapping.mapping,
            SourceMapping::NonRequirement { reason }
                | SourceMapping::SupersededContext { reason }
                if reason.trim().is_empty()
        ) {
            return Err("source mapping reason is blank".to_string());
        }
    }

    let mut manifest_keys = BTreeSet::new();
    let mut all_requirement_ids = BTreeSet::new();
    for manifest in &ledger.manifest_snapshots {
        if !manifest_keys.insert((manifest.completion_epoch, manifest.manifest_revision))
            || manifest.manifest_hash
                != requirement_manifest_hash(manifest.manifest_revision, &manifest.requirements)
        {
            return Err(
                "requirement manifest revision is duplicated or has a bad hash".to_string(),
            );
        }
        let mut ids = BTreeSet::new();
        for requirement in &manifest.requirements {
            let Some(source) = ledger.source_records.get(&requirement.source_id) else {
                return Err("requirement references an unknown source".to_string());
            };
            if source.completion_epoch != manifest.completion_epoch
                || source.content_hash != requirement.source_content_hash
                || material_for_span(source, &requirement.source_span).as_deref()
                    != Some(requirement.exact_material.as_str())
                || requirement.requirement_id
                    != deterministic_requirement_id(source, &requirement.source_span)
                || !ids.insert(requirement.requirement_id.clone())
            {
                return Err("requirement provenance, span, or identity is inconsistent".to_string());
            }
            match requirement.status {
                RequirementStatus::Active | RequirementStatus::Withdrawn
                    if requirement.superseded_by.is_some() =>
                {
                    return Err("active or withdrawn requirement has superseded_by".to_string());
                }
                RequirementStatus::Superseded if requirement.superseded_by.is_none() => {
                    return Err("superseded requirement lacks superseded_by".to_string());
                }
                _ => {}
            }
            all_requirement_ids.insert(requirement.requirement_id.clone());
        }
        if manifest.requirements.iter().any(|requirement| {
            requirement
                .superseded_by
                .as_ref()
                .is_some_and(|requirement_id| !ids.contains(requirement_id))
        }) {
            return Err("superseded_by points outside its manifest".to_string());
        }
        if !requirement_supersession_is_acyclic(&manifest.requirements) {
            return Err("requirement supersession graph contains a cycle".to_string());
        }
    }

    if ledger.manifest_revision > 0 {
        let active_manifest = active_manifest(ledger)
            .ok_or_else(|| "active manifest revision is missing".to_string())?;
        let active_mappings = active_source_mappings(ledger);
        let active_source_ids = ledger
            .source_records
            .values()
            .filter(|source| source.completion_epoch == ledger.completion_epoch)
            .map(|source| source.source_id.as_str())
            .collect::<BTreeSet<_>>();
        if active_mappings
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != active_source_ids
        {
            return Err("active source mapping set is incomplete".to_string());
        }
        let ids_by_source = active_manifest.requirements.iter().fold(
            BTreeMap::<String, Vec<String>>::new(),
            |mut result, requirement| {
                result
                    .entry(requirement.source_id.clone())
                    .or_default()
                    .push(requirement.requirement_id.clone());
                result
            },
        );
        for (source_id, mapping) in active_mappings {
            if let SourceMapping::RequirementBearing {
                mut requirement_ids,
            } = mapping
            {
                requirement_ids.sort();
                let mut expected = ids_by_source.get(&source_id).cloned().unwrap_or_default();
                expected.sort();
                if requirement_ids != expected {
                    return Err("active source mapping is not two-way consistent".to_string());
                }
            } else if ids_by_source.contains_key(&source_id) {
                return Err("requirement-bearing source has a non-requirement mapping".to_string());
            }
        }
    }

    validate_v5_revision_snapshots(ledger)?;
    validate_v5_review_receipts(document, ledger)?;
    Ok(())
}

fn validate_v5_revision_snapshots(ledger: &CompletionReviewLedgerV2) -> Result<(), String> {
    let mut revisions_by_epoch = BTreeMap::<u64, BTreeSet<u64>>::new();
    for manifest in &ledger.manifest_snapshots {
        if manifest.completion_epoch == 0
            || manifest.completion_epoch > ledger.completion_epoch
            || manifest.manifest_revision == 0
        {
            return Err("manifest snapshot epoch or revision is invalid".to_string());
        }
        revisions_by_epoch
            .entry(manifest.completion_epoch)
            .or_default()
            .insert(manifest.manifest_revision);
    }
    for (epoch, revisions) in &revisions_by_epoch {
        let max_revision = revisions.iter().next_back().copied().unwrap_or_default();
        let expected = (1..=max_revision).collect::<BTreeSet<_>>();
        if revisions != &expected {
            return Err("manifest revisions are not contiguous within an epoch".to_string());
        }
        for revision in revisions {
            let manifest = ledger
                .manifest_snapshots
                .iter()
                .find(|manifest| {
                    manifest.completion_epoch == *epoch && manifest.manifest_revision == *revision
                })
                .ok_or_else(|| "manifest revision is missing".to_string())?;
            let mappings = source_mappings_for(ledger, *epoch, *revision);
            let expected_sources = ledger
                .source_records
                .values()
                .filter(|source| {
                    source.completion_epoch == *epoch
                        && source.introduced_manifest_revision > 0
                        && source.introduced_manifest_revision <= *revision
                })
                .map(|source| source.source_id.as_str())
                .collect::<BTreeSet<_>>();
            if mappings.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_sources {
                return Err("historical source mapping set is incomplete".to_string());
            }
            let ids_by_source = manifest.requirements.iter().fold(
                BTreeMap::<String, Vec<String>>::new(),
                |mut result, requirement| {
                    result
                        .entry(requirement.source_id.clone())
                        .or_default()
                        .push(requirement.requirement_id.clone());
                    result
                },
            );
            for (source_id, mapping) in mappings {
                let mut expected_ids = ids_by_source.get(&source_id).cloned().unwrap_or_default();
                expected_ids.sort();
                match mapping {
                    SourceMapping::RequirementBearing {
                        mut requirement_ids,
                    } => {
                        requirement_ids.sort();
                        requirement_ids.dedup();
                        if requirement_ids != expected_ids {
                            return Err(
                                "historical source mapping is not two-way consistent".to_string()
                            );
                        }
                    }
                    SourceMapping::PendingClassification
                    | SourceMapping::NonRequirement { .. }
                    | SourceMapping::SupersededContext { .. }
                    | SourceMapping::UnavailableOrTruncated => {
                        if !expected_ids.is_empty() {
                            return Err(
                                "historical requirement-bearing source has an incompatible mapping"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
        }
    }
    if ledger.manifest_revision > 0
        && revisions_by_epoch
            .get(&ledger.completion_epoch)
            .and_then(|revisions| revisions.iter().next_back().copied())
            != Some(ledger.manifest_revision)
    {
        return Err("active manifest revision is not the latest revision in its epoch".to_string());
    }
    for source in ledger.source_records.values() {
        if source.introduced_manifest_revision == 0
            || revisions_by_epoch
                .get(&source.completion_epoch)
                .is_none_or(|revisions| !revisions.contains(&source.introduced_manifest_revision))
        {
            return Err("source introduction revision is missing or invalid".to_string());
        }
    }
    let max_source_ordinal = ledger
        .source_records
        .values()
        .map(|source| source.source_ordinal)
        .max()
        .unwrap_or_default();
    if ledger.next_source_ordinal <= max_source_ordinal {
        return Err("next source ordinal can reuse a persisted source identity".to_string());
    }
    Ok(())
}

fn parse_review_coordinates(review_id: &str) -> Option<(u64, u64, u64)> {
    let mut fields = review_id.strip_prefix("review-")?.split('-');
    let epoch = fields.next()?.parse().ok()?;
    let revision = fields.next()?.parse().ok()?;
    let sequence = fields.next()?.parse().ok()?;
    fields
        .next()
        .is_none()
        .then_some((epoch, revision, sequence))
}

fn receipt_identity_matches(
    left: &CompletionReviewReceiptV2,
    right: &CompletionReviewReceiptV2,
) -> bool {
    left.candidate_mutation_revision == right.candidate_mutation_revision
        && left.candidate_hash == right.candidate_hash
        && left.implementation_identity_hash == right.implementation_identity_hash
        && left.dossier_snapshot_id == right.dossier_snapshot_id
        && left.user_source_ledger_hash == right.user_source_ledger_hash
        && left.requirement_manifest_hash == right.requirement_manifest_hash
}

fn manifest_gap_supersession_is_valid(
    receipt: &CompletionReviewReceiptV2,
    superseded: &CompletionReviewReceiptV2,
) -> bool {
    let Some((epoch, revision, _)) = parse_review_coordinates(&receipt.review_id) else {
        return false;
    };
    let Some((superseded_epoch, superseded_revision, _)) =
        parse_review_coordinates(&superseded.review_id)
    else {
        return false;
    };
    receipt.attempt_kind == CompletionReviewAttemptKind::InitialReview
        && matches!(
            superseded.attempt_kind,
            CompletionReviewAttemptKind::InitialReview | CompletionReviewAttemptKind::Rereview
        )
        && superseded.infrastructure_outcome == "ok"
        && !superseded.review_clean
        && superseded.terminal_outcome.is_none()
        && !superseded.manifest_gaps.is_empty()
        && epoch == superseded_epoch
        && superseded_revision < revision
}

fn is_strictly_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn structured_surface_is_canonical(surface: &StructuredContractSurface) -> bool {
    !surface.kind.trim().is_empty()
        && surface.kind == surface.kind.trim()
        && !surface.owner.trim().is_empty()
        && surface.owner == surface.owner.trim()
        && !surface.identifier.trim().is_empty()
        && surface.identifier == surface.identifier.trim()
}

fn validate_persisted_repair_baseline(
    baseline: &RepairBaseline,
    initial_receipt: &CompletionReviewReceiptV2,
    manifest: &RequirementManifestSnapshot,
) -> Result<(), String> {
    if baseline.source_ledger_hash != initial_receipt.user_source_ledger_hash
        || baseline.requirement_manifest_hash != initial_receipt.requirement_manifest_hash
        || baseline.plan_structure_hash.trim().is_empty()
        || baseline.repair_scope.path_grammar_version != REPAIR_PATH_GRAMMAR_VERSION
    {
        return Err("repair baseline identity or grammar is invalid".to_string());
    }

    if baseline
        .path_states
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
        || baseline.path_states.iter().any(|state| {
            canonical_repair_path(&state.path, false).as_deref() != Ok(state.path.as_str())
                || state.exists != state.content_hash.is_some()
                || state.content_hash.as_deref().is_some_and(str::is_empty)
        })
    {
        return Err("repair baseline path states are not canonical".to_string());
    }

    let scope_ids = baseline
        .repair_scope
        .paths
        .iter()
        .map(path_scope_identifier)
        .collect::<Vec<_>>();
    if !is_strictly_sorted_unique(&scope_ids)
        || baseline.repair_scope.paths.iter().any(|scope| match scope {
            RepairPathScope::ExactFile { path } | RepairPathScope::DirectoryPrefix { path } => {
                canonical_repair_path(path, false).as_deref() != Ok(path.as_str())
            }
            RepairPathScope::GeneratedPattern {
                grammar_version,
                pattern,
            } => {
                !generated_pattern_is_supported(pattern, *grammar_version)
                    || canonical_repair_path(pattern, true).as_deref() != Ok(pattern.as_str())
            }
        })
    {
        return Err("repair baseline path scope is not canonical".to_string());
    }

    if !is_strictly_sorted_unique(&baseline.implementation_surfaces)
        || !is_strictly_sorted_unique(&baseline.repair_scope.surfaces)
        || baseline
            .implementation_surfaces
            .iter()
            .chain(baseline.repair_scope.surfaces.iter())
            .any(|surface| !structured_surface_is_canonical(surface))
        || baseline
            .implementation_surfaces
            .iter()
            .any(|surface| !baseline.repair_scope.surfaces.contains(surface))
    {
        return Err("repair baseline contract surfaces are not canonical".to_string());
    }

    if baseline
        .command_bindings
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
        || baseline
            .command_bindings
            .iter()
            .any(|binding| binding.receipt_id.trim().is_empty())
        || baseline
            .command_bindings
            .iter()
            .map(|binding| binding.receipt_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != baseline.command_bindings.len()
        || baseline.command_sequence_high_water_mark
            != baseline
                .command_bindings
                .last()
                .map(|binding| binding.sequence)
                .unwrap_or(0)
    {
        return Err("repair baseline command sequence is not canonical".to_string());
    }

    for identities in [
        &baseline.default_child_mutation_identities,
        &baseline.typed_mutation_identities,
        &baseline.external_evidence_ids,
    ] {
        if !is_strictly_sorted_unique(identities)
            || identities.iter().any(|identity| identity.trim().is_empty())
        {
            return Err("repair baseline evidence identities are not canonical".to_string());
        }
    }

    let active_requirement_ids = manifest
        .requirements
        .iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .map(|requirement| requirement.requirement_id.clone())
        .collect::<BTreeSet<_>>();
    let expected_affected =
        derive_affected_requirement_ids(&active_requirement_ids, &initial_receipt.findings)
            .map_err(|_| {
                "repair baseline references an inactive or unknown requirement".to_string()
            })?;
    if baseline.repair_scope.affected_requirement_ids != expected_affected {
        return Err("repair baseline affected requirements are invalid".to_string());
    }
    Ok(())
}

fn validate_repair_delta_contents(
    delta: &RepairDelta,
    baseline: &RepairBaseline,
    original_findings: &[CompletionReviewFindingReceipt],
) -> Result<(), String> {
    let expected_disposition_ids = original_findings
        .iter()
        .map(|finding| finding.finding_id.clone())
        .collect::<Vec<_>>();
    if delta.original_findings != original_findings
        || delta.required_disposition_finding_ids != expected_disposition_ids
        || delta.affected_requirement_ids != baseline.repair_scope.affected_requirement_ids
    {
        return Err("repair delta review obligations are invalid".to_string());
    }

    if delta
        .path_changes
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
        || delta.path_changes.iter().any(|change| {
            canonical_repair_path(&change.path, false).as_deref() != Ok(change.path.as_str())
                || !baseline
                    .repair_scope
                    .paths
                    .iter()
                    .any(|scope| repair_scope_matches(scope, &change.path) == Ok(true))
                || change.before_exists != change.before_hash.is_some()
                || change.after_exists != change.after_hash.is_some()
                || match &change.change {
                    RepairPathChangeKind::Added => change.before_exists || !change.after_exists,
                    RepairPathChangeKind::Removed => !change.before_exists || change.after_exists,
                    RepairPathChangeKind::Modified => {
                        !change.before_exists
                            || !change.after_exists
                            || change.before_hash == change.after_hash
                    }
                }
        })
    {
        return Err("repair delta path changes are not canonical".to_string());
    }

    if delta
        .new_command_receipts
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
        || delta.new_command_receipts.iter().any(|receipt| {
            receipt.sequence <= baseline.command_sequence_high_water_mark
                || receipt.receipt_id.trim().is_empty()
        })
        || delta
            .new_command_receipts
            .iter()
            .map(|receipt| receipt.receipt_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != delta.new_command_receipts.len()
    {
        return Err("repair delta command receipts are not canonical".to_string());
    }

    if delta
        .invalidated_command_receipts
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
        || delta.invalidated_command_receipts.iter().any(|receipt| {
            let Some(binding) = baseline
                .command_bindings
                .iter()
                .find(|binding| binding.sequence == receipt.sequence)
            else {
                return true;
            };
            binding.receipt_id != receipt.receipt_id
                || receipt.reason != "implementation_identity_binding_mismatch"
                || binding.implementation_identity.as_deref()
                    == Some(delta.candidate_implementation_identity.as_str())
        })
    {
        return Err("repair delta command invalidations are invalid".to_string());
    }

    if !is_strictly_sorted_unique(&delta.newly_realized_surfaces)
        || delta.newly_realized_surfaces.iter().any(|surface| {
            !structured_surface_is_canonical(surface)
                || baseline.implementation_surfaces.contains(surface)
                || !baseline.repair_scope.surfaces.contains(surface)
        })
    {
        return Err("repair delta contract surfaces are invalid".to_string());
    }
    Ok(())
}

fn validate_rereview_audit_metadata(
    receipt: &CompletionReviewReceiptV2,
    parent: Option<&CompletionReviewReceiptV2>,
    manifest: Option<&RequirementManifestSnapshot>,
) -> Result<(), String> {
    let has_audit_metadata = receipt.repair_baseline.is_some()
        || receipt.baseline_hash.is_some()
        || receipt.input_mode.is_some()
        || receipt.delta_hash.is_some()
        || receipt.rereview_delta.is_some()
        || !receipt.fallback_reasons.is_empty()
        || receipt.candidate_implementation_identity.is_some()
        || receipt.rereview_audit_hash.is_some();

    match receipt.attempt_kind {
        CompletionReviewAttemptKind::InitialReview => {
            if receipt.input_mode.is_some()
                || receipt.delta_hash.is_some()
                || receipt.rereview_delta.is_some()
                || !receipt.fallback_reasons.is_empty()
                || receipt.candidate_implementation_identity.is_some()
                || receipt.rereview_audit_hash.is_some()
            {
                return Err("initial review contains rereview audit metadata".to_string());
            }
            match (
                receipt.repair_instruction_hash.as_ref(),
                receipt.repair_baseline.as_ref(),
                receipt.baseline_hash.as_ref(),
            ) {
                (None, None, None) | (Some(_), None, None) => Ok(()),
                (Some(_), Some(baseline), Some(baseline_hash)) => {
                    let manifest = manifest.ok_or_else(|| {
                        "repair baseline has no exact requirement manifest".to_string()
                    })?;
                    validate_persisted_repair_baseline(baseline, receipt, manifest)?;
                    if repair_baseline_hash(baseline) != *baseline_hash {
                        return Err("repair baseline hash is invalid".to_string());
                    }
                    Ok(())
                }
                _ => Err("initial review repair baseline metadata is incomplete".to_string()),
            }
        }
        CompletionReviewAttemptKind::CorrectionEvidence
        | CompletionReviewAttemptKind::TerminalClosure => {
            if has_audit_metadata {
                Err("non-review receipt contains repair-delta audit metadata".to_string())
            } else {
                Ok(())
            }
        }
        CompletionReviewAttemptKind::Rereview => {
            if !has_audit_metadata {
                return Ok(());
            }
            if receipt.repair_baseline.is_some() {
                return Err("rereview duplicates the initial repair baseline".to_string());
            }
            let parent = parent
                .filter(|parent| parent.attempt_kind == CompletionReviewAttemptKind::InitialReview)
                .ok_or_else(|| "rereview audit metadata lacks its initial parent".to_string())?;
            let input_mode = receipt
                .input_mode
                .ok_or_else(|| "rereview input mode is missing".to_string())?;
            let candidate_identity = receipt
                .candidate_implementation_identity
                .as_ref()
                .ok_or_else(|| "rereview candidate identity is missing".to_string())?;
            let repair_instruction_hash = receipt
                .repair_instruction_hash
                .as_ref()
                .ok_or_else(|| "rereview repair instruction hash is missing".to_string())?;
            if candidate_identity != &receipt.implementation_identity_hash
                || parent.repair_instruction_hash.as_ref() != Some(repair_instruction_hash)
                || !is_strictly_sorted_unique(&receipt.fallback_reasons)
            {
                return Err("rereview audit lineage is invalid".to_string());
            }

            match input_mode {
                RereviewInputMode::Delta => {
                    let baseline = parent.repair_baseline.as_ref().ok_or_else(|| {
                        "delta rereview parent has no repair baseline".to_string()
                    })?;
                    let parent_baseline_hash = parent
                        .baseline_hash
                        .as_ref()
                        .ok_or_else(|| "delta rereview parent has no baseline hash".to_string())?;
                    if receipt.baseline_hash.as_ref() != Some(parent_baseline_hash)
                        || repair_baseline_hash(baseline) != *parent_baseline_hash
                        || !receipt.fallback_reasons.is_empty()
                    {
                        return Err("delta rereview baseline binding is invalid".to_string());
                    }
                    let delta = receipt
                        .rereview_delta
                        .as_ref()
                        .ok_or_else(|| "delta rereview payload is missing".to_string())?;
                    let delta_hash = receipt
                        .delta_hash
                        .as_ref()
                        .ok_or_else(|| "delta rereview hash is missing".to_string())?;
                    if repair_delta_hash(delta) != *delta_hash
                        || delta.baseline_hash != *parent_baseline_hash
                        || delta.repair_instruction_hash != *repair_instruction_hash
                        || delta.candidate_implementation_identity != *candidate_identity
                    {
                        return Err("delta rereview binding is invalid".to_string());
                    }
                    validate_repair_delta_contents(delta, baseline, &parent.findings)?;
                }
                RereviewInputMode::FullFallback => {
                    if receipt.delta_hash.is_some()
                        || receipt.rereview_delta.is_some()
                        || receipt.fallback_reasons.is_empty()
                        || receipt.baseline_hash != parent.baseline_hash
                    {
                        return Err("full-fallback rereview metadata is invalid".to_string());
                    }
                }
            }

            let input = RereviewInput {
                input_mode,
                baseline_hash: receipt.baseline_hash.clone(),
                delta_hash: receipt.delta_hash.clone(),
                fallback_reasons: receipt.fallback_reasons.clone(),
                repair_instruction_hash: repair_instruction_hash.clone(),
                candidate_implementation_identity: candidate_identity.clone(),
                delta: receipt.rereview_delta.clone(),
            };
            let audit_hash = rereview_audit_hash(&input);
            if receipt.rereview_audit_hash.as_ref() != Some(&audit_hash) {
                return Err("rereview audit hash is invalid".to_string());
            }
            Ok(())
        }
    }
}

fn validate_v5_review_receipts(
    document: &TaskEvidenceDocument,
    ledger: &CompletionReviewLedgerV2,
) -> Result<(), String> {
    let manifest_by_key = ledger
        .manifest_snapshots
        .iter()
        .map(|manifest| {
            (
                (manifest.completion_epoch, manifest.manifest_revision),
                manifest,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeMap::<String, &CompletionReviewReceiptV2>::new();
    let mut max_sequence = 0;
    for (receipt_index, receipt) in ledger.receipts.iter().enumerate() {
        if receipt.review_id.trim().is_empty() || seen.contains_key(&receipt.review_id) {
            return Err("review receipt ID is blank or duplicated".to_string());
        }
        let migrated_v4 = receipt.infrastructure_outcome == "migrated_v4_terminal";
        let coordinates = parse_review_coordinates(&receipt.review_id);
        let manifest = if migrated_v4 {
            None
        } else {
            let (epoch, revision, sequence) =
                coordinates.ok_or_else(|| "review receipt ID is not host-canonical".to_string())?;
            max_sequence = max_sequence.max(sequence);
            let manifest = manifest_by_key
                .get(&(epoch, revision))
                .copied()
                .ok_or_else(|| "review receipt has no exact manifest snapshot".to_string())?;
            if receipt.requirement_manifest_hash != manifest.manifest_hash {
                return Err("review receipt is bound to the wrong manifest hash".to_string());
            }
            let source_hash_matches = [false, true].into_iter().any(|source_capture_failed| {
                receipt.user_source_ledger_hash
                    == user_source_ledger_snapshot_hash(
                        ledger,
                        epoch,
                        revision,
                        source_capture_failed,
                    )
            });
            if !source_hash_matches {
                return Err("review receipt is bound to the wrong user-source snapshot".to_string());
            }
            Some(manifest)
        };
        if receipt.candidate_hash != receipt.implementation_identity_hash
            || receipt.implementation_identity_hash.trim().is_empty()
            || receipt.dossier_snapshot_id.trim().is_empty()
            || receipt.infrastructure_outcome.trim().is_empty()
        {
            return Err("review receipt identity or infrastructure outcome is invalid".to_string());
        }
        if (receipt.disposition == CompletionReviewDisposition::Attempted)
            != receipt.attempted_outcome.is_some()
        {
            return Err(
                "attempted-review outcome is inconsistent with its disposition".to_string(),
            );
        }
        match receipt.attempted_outcome {
            Some(CompletionReviewAttemptedOutcome::Clean)
                if !receipt.review_clean || !receipt.findings.is_empty() =>
            {
                return Err("clean review outcome has findings or is not marked clean".to_string());
            }
            Some(CompletionReviewAttemptedOutcome::ActionableFindings)
                if (receipt.findings.is_empty() && receipt.manifest_gaps.is_empty())
                    || receipt.infrastructure_outcome != "ok" =>
            {
                return Err(
                    "actionable review outcome lacks findings or has infrastructure failure"
                        .to_string(),
                );
            }
            Some(CompletionReviewAttemptedOutcome::InfrastructureFailure)
                if receipt.infrastructure_outcome == "ok" =>
            {
                return Err(
                    "infrastructure review outcome is recorded with successful infrastructure"
                        .to_string(),
                );
            }
            _ => {}
        }
        let parent =
            match receipt.parent_review_id.as_ref() {
                Some(parent_id) => Some(*seen.get(parent_id).ok_or_else(|| {
                    "review receipt parent is missing or not earlier".to_string()
                })?),
                None => None,
            };
        let superseded = match receipt.superseded_review_id.as_ref() {
            Some(review_id) => Some(
                *seen
                    .get(review_id)
                    .ok_or_else(|| "superseded review is missing or not earlier".to_string())?,
            ),
            None => None,
        };
        let active_requirement_ids = manifest
            .map(|manifest| {
                manifest
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.status == RequirementStatus::Active)
                    .map(|requirement| requirement.requirement_id.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let valid_requirement_ids = manifest
            .map(|manifest| {
                manifest
                    .requirements
                    .iter()
                    .map(|requirement| requirement.requirement_id.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        for (index, finding) in receipt.findings.iter().enumerate() {
            if finding.finding_id != format!("{}/F{}", receipt.review_id, index + 1)
                || finding
                    .requirement_ids
                    .iter()
                    .any(|requirement_id| !valid_requirement_ids.contains(requirement_id.as_str()))
                || (!finding.requirement_ids.is_empty()
                    && !finding.requirement_ids.iter().any(|requirement_id| {
                        active_requirement_ids.contains(requirement_id.as_str())
                    }))
                || !COMPLETION_REVIEW_LENSES.contains(&finding.lens.as_str())
                || finding.contract_surface.trim().is_empty()
                || finding.severity.trim().is_empty()
                || finding.evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.proof_route.trim().is_empty()
            {
                return Err("review finding is not canonical or receipt-local".to_string());
            }
        }
        if receipt.review_clean && !receipt.findings.is_empty() {
            return Err("clean review receipt contains findings".to_string());
        }
        if receipt.manifest_gaps.iter().any(|gap| {
            let Some(source) = ledger.source_records.get(&gap.source_id) else {
                return true;
            };
            gap.omitted_spans.is_empty()
                || gap.omitted_spans.iter().any(|span| {
                    let Some(material) = material_for_span(source, span) else {
                        return true;
                    };
                    material.trim().is_empty()
                        || manifest.is_some_and(|manifest| {
                            let requirement_id = deterministic_requirement_id(source, span);
                            manifest
                                .requirements
                                .iter()
                                .any(|requirement| requirement.requirement_id == requirement_id)
                        })
                })
        }) {
            return Err(
                "manifest gap does not identify omitted immutable source material".to_string(),
            );
        }
        validate_rereview_audit_metadata(receipt, parent, manifest)?;
        match receipt.attempt_kind {
            CompletionReviewAttemptKind::InitialReview => {
                if !receipt.dispositions.is_empty()
                    || parent.is_some_and(|parent| {
                        parent.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
                    })
                    || superseded.is_some_and(|superseded| {
                        !manifest_gap_supersession_is_valid(receipt, superseded)
                    })
                {
                    return Err("initial review lineage is invalid".to_string());
                }
            }
            CompletionReviewAttemptKind::CorrectionEvidence => {
                let Some(parent) = parent else {
                    return Err("correction evidence lacks its initial-review parent".to_string());
                };
                if parent.attempt_kind != CompletionReviewAttemptKind::InitialReview
                    || parent.repair_instruction_hash.is_none()
                    || receipt.repair_instruction_hash != parent.repair_instruction_hash
                    || !receipt.findings.is_empty()
                    || !receipt.dispositions.is_empty()
                    || !receipt.manifest_gaps.is_empty()
                    || receipt.review_clean
                    || receipt.terminal_outcome.is_some()
                {
                    return Err("correction evidence lineage is invalid".to_string());
                }
            }
            CompletionReviewAttemptKind::Rereview => {
                let Some(parent) = parent else {
                    return Err("rereview lacks its initial-review parent".to_string());
                };
                if parent.attempt_kind != CompletionReviewAttemptKind::InitialReview {
                    return Err("rereview parent or manifest-gap state is invalid".to_string());
                }
                let correction = ledger.receipts[..receipt_index]
                    .iter()
                    .rev()
                    .find(|candidate| {
                        candidate.attempt_kind == CompletionReviewAttemptKind::CorrectionEvidence
                            && candidate.parent_review_id.as_deref()
                                == Some(parent.review_id.as_str())
                    })
                    .ok_or_else(|| "rereview lacks correction evidence".to_string())?;
                if correction.infrastructure_outcome != "ok"
                    || correction.terminal_outcome.is_some()
                    || correction.repair_instruction_hash != parent.repair_instruction_hash
                    || receipt.repair_instruction_hash != parent.repair_instruction_hash
                    || !receipt_identity_matches(receipt, correction)
                {
                    return Err("rereview is not bound to its correction evidence".to_string());
                }
                if receipt.infrastructure_outcome == "ok" {
                    let expected = parent
                        .findings
                        .iter()
                        .map(|finding| finding.finding_id.as_str())
                        .collect::<BTreeSet<_>>();
                    let returned = receipt
                        .dispositions
                        .iter()
                        .map(|disposition| disposition.finding_id.as_str())
                        .collect::<BTreeSet<_>>();
                    if returned.len() != receipt.dispositions.len()
                        || returned != expected
                        || receipt.dispositions.iter().any(|disposition| {
                            !matches!(
                                disposition.disposition.as_str(),
                                "resolved"
                                    | "rebuttal_accepted"
                                    | "still_present"
                                    | "insufficient_proof"
                                    | "regressed"
                            ) || disposition.evidence.trim().is_empty()
                        })
                    {
                        return Err(
                            "rereview dispositions do not exactly cover original findings"
                                .to_string(),
                        );
                    }
                } else if !receipt.findings.is_empty()
                    || !receipt.dispositions.is_empty()
                    || !receipt.manifest_gaps.is_empty()
                    || receipt.review_clean
                    || receipt.terminal_outcome.is_some()
                    || superseded.is_some()
                {
                    return Err(
                        "failed rereview receipt contains reviewer-authored or terminal state"
                            .to_string(),
                    );
                }
            }
            CompletionReviewAttemptKind::TerminalClosure => {
                let Some(parent) = parent else {
                    return Err("terminal closure lacks its accepted review parent".to_string());
                };
                let infrastructure_outcome_is_valid = receipt.infrastructure_outcome == "ok"
                    || (migrated_v4 && parent.infrastructure_outcome == "migrated_v4_terminal");
                if !matches!(
                    parent.attempt_kind,
                    CompletionReviewAttemptKind::InitialReview
                        | CompletionReviewAttemptKind::Rereview
                ) || parent.terminal_outcome.is_some()
                    || !receipt_identity_matches(receipt, parent)
                    || !receipt.findings.is_empty()
                    || !receipt.dispositions.is_empty()
                    || !receipt.manifest_gaps.is_empty()
                    || receipt.repair_instruction_hash.is_some()
                    || !infrastructure_outcome_is_valid
                {
                    return Err("terminal closure is not bound to its exact review".to_string());
                }
                match receipt.terminal_outcome.as_deref() {
                    Some("passed")
                        if receipt.review_clean && parent.review_clean && superseded.is_none() => {}
                    Some("partial") if !receipt.review_clean => {
                        if let Some(superseded) = superseded
                            && (superseded.attempt_kind
                                != CompletionReviewAttemptKind::TerminalClosure
                                || superseded.terminal_outcome.as_deref() != Some("passed")
                                || !receipt_identity_matches(receipt, superseded))
                        {
                            return Err(
                                "partial terminal closure supersession is invalid".to_string()
                            );
                        }
                    }
                    Some("blocked") if !receipt.review_clean && superseded.is_none() => {}
                    _ => return Err("terminal closure outcome is invalid".to_string()),
                }
            }
        }
        seen.insert(receipt.review_id.clone(), receipt);
    }
    if ledger.next_review_sequence <= max_sequence {
        return Err("next review sequence can reuse a persisted review identity".to_string());
    }
    if let Some(cycle) = ledger.active_review_cycle.as_ref() {
        if cycle.manifest_revision != ledger.manifest_revision {
            return Err("active review cycle targets the wrong manifest revision".to_string());
        }
        if let Some(parent_terminal) = cycle.parent_terminal_review_id.as_ref()
            && seen.get(parent_terminal).is_none_or(|receipt| {
                receipt.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
            })
        {
            return Err("active cycle parent terminal receipt is invalid".to_string());
        }
        if let Some(superseded_review_id) = cycle.superseded_review_id.as_ref() {
            let superseded = seen
                .get(superseded_review_id)
                .copied()
                .ok_or_else(|| "active cycle superseded review is missing".to_string())?;
            let Some((epoch, revision, _)) = parse_review_coordinates(&superseded.review_id) else {
                return Err("active cycle superseded review ID is invalid".to_string());
            };
            if !matches!(
                superseded.attempt_kind,
                CompletionReviewAttemptKind::InitialReview | CompletionReviewAttemptKind::Rereview
            ) || superseded.manifest_gaps.is_empty()
                || epoch != ledger.completion_epoch
                || revision >= cycle.manifest_revision
            {
                return Err("active cycle manifest-gap supersession is invalid".to_string());
            }
            if let Some(current_initial) = ledger.receipts.iter().rev().find(|receipt| {
                receipt.attempt_kind == CompletionReviewAttemptKind::InitialReview
                    && parse_review_coordinates(&receipt.review_id).is_some_and(
                        |(receipt_epoch, receipt_revision, _)| {
                            receipt_epoch == ledger.completion_epoch
                                && receipt_revision == cycle.manifest_revision
                        },
                    )
            }) && current_initial.superseded_review_id.as_deref()
                != Some(superseded_review_id.as_str())
            {
                return Err(
                    "replacement initial review lost its manifest-gap supersession".to_string(),
                );
            }
        }
        match cycle.accepted_review_id.as_ref() {
            Some(accepted_id) => {
                let accepted = seen
                    .get(accepted_id)
                    .copied()
                    .ok_or_else(|| "active cycle accepted review is missing".to_string())?;
                if !accepted.review_clean
                    || accepted.terminal_outcome.is_some()
                    || !matches!(
                        accepted.attempt_kind,
                        CompletionReviewAttemptKind::InitialReview
                            | CompletionReviewAttemptKind::Rereview
                    )
                    || cycle.accepted_dossier_snapshot_id.as_deref()
                        != Some(accepted.dossier_snapshot_id.as_str())
                {
                    return Err("active cycle accepted review pointer is inconsistent".to_string());
                }
            }
            None if cycle.accepted_dossier_snapshot_id.is_some() => {
                return Err(
                    "active cycle has a dossier pointer without an accepted review".to_string(),
                );
            }
            None => {}
        }
        if matches!(
            cycle.phase,
            CompletionReviewCyclePhase::ProvisionalClean | CompletionReviewCyclePhase::Closed
        ) && cycle.accepted_review_id.is_none()
        {
            return Err("clean or closed cycle lacks an accepted review".to_string());
        }
    }
    let completion_passed = document
        .completion
        .as_ref()
        .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
    if completion_passed {
        let cycle = ledger
            .active_review_cycle
            .as_ref()
            .ok_or_else(|| "Passed completion lacks an active cycle".to_string())?;
        let accepted_id = cycle
            .accepted_review_id
            .as_ref()
            .ok_or_else(|| "Passed completion lacks an accepted review".to_string())?;
        let terminal_id = ledger
            .last_terminal_closure
            .as_ref()
            .ok_or_else(|| "Passed completion lacks terminal closure".to_string())?;
        let terminal = seen
            .get(terminal_id)
            .copied()
            .ok_or_else(|| "Passed terminal closure is missing".to_string())?;
        if cycle.phase != CompletionReviewCyclePhase::Closed
            || ledger.review_risk.unresolved
            || ledger.review_risk.resolved_at.is_none()
            || terminal.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
            || terminal.terminal_outcome.as_deref() != Some("passed")
            || terminal.parent_review_id.as_ref() != Some(accepted_id)
            || seen
                .get(accepted_id)
                .is_none_or(|accepted| !receipt_identity_matches(terminal, accepted))
        {
            return Err("Passed completion lacks an exact atomic terminal closure".to_string());
        }
    }
    if !ledger.review_risk.unresolved
        && ledger.review_risk.resolved_at.is_some()
        && ledger.last_terminal_closure.is_none()
    {
        return Err("resolved completion-review risk lacks terminal closure".to_string());
    }
    Ok(())
}

fn uses_retired_v3_completion_shape(schema_version: u32, value: &Value) -> bool {
    schema_version == 3
        && [
            "validation_epoch",
            "next_validation_receipt_sequence",
            "validation_receipts",
            "wiring_receipt",
        ]
        .iter()
        .any(|field| value.get(*field).is_some())
}

fn recorded_repository_root_matches(recorded: &str, expected: &Path) -> bool {
    let recorded = Path::new(recorded);
    recorded.is_absolute() && repository_roots_match(recorded, expected)
}

fn canonical_repository_root(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn repository_roots_match(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(left), Ok(right)) => repository_root_paths_equal(&left, &right),
        _ => repository_root_paths_equal(left, right),
    }
}

fn recorded_path_uri_matches(recorded: &str, expected: &Path) -> bool {
    PathUri::parse(recorded)
        .ok()
        .and_then(|uri| uri.to_abs_path().ok())
        .is_some_and(|path| repository_roots_match(path.as_path(), expected))
}

#[cfg(not(windows))]
fn repository_root_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(windows)]
fn repository_root_paths_equal(left: &Path, right: &Path) -> bool {
    use std::path::Component;
    use std::path::Prefix;

    let mut left_components = left.components();
    let mut right_components = right.components();
    let left_drive = match left_components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => Some(drive),
            _ => None,
        },
        _ => None,
    };
    let right_drive = match right_components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => Some(drive),
            _ => None,
        },
        _ => None,
    };

    match (left_drive, right_drive) {
        (Some(left_drive), Some(right_drive)) => {
            left_drive.eq_ignore_ascii_case(&right_drive) && left_components.eq(right_components)
        }
        _ => left == right,
    }
}

async fn quarantine_evidence_file(path: &Path, kind: &str) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("task-evidence.json");
    let quarantine = path.with_file_name(format!(
        "{file_name}.{kind}.{}.preserved",
        uuid::Uuid::now_v7()
    ));
    tokio::fs::rename(path, &quarantine).await?;
    Ok(quarantine)
}

#[cfg(test)]
fn migrate_document(document: &mut TaskEvidenceDocument) {
    let legacy_completion_model = document.schema_version < TASK_EVIDENCE_COMPLETION_MODEL_VERSION;
    migrate_document_with_completion_model(document, legacy_completion_model);
}

fn migrate_document_with_completion_model(
    document: &mut TaskEvidenceDocument,
    legacy_completion_model: bool,
) {
    let source_schema_version = document.schema_version;
    document.next_edit_receipt_sequence =
        document
            .next_edit_receipt_sequence
            .max(next_sequence_after_ids(
                document
                    .edit_receipts
                    .iter()
                    .map(|receipt| receipt.id.as_str()),
            ));
    document.next_command_receipt_sequence =
        document
            .next_command_receipt_sequence
            .max(next_sequence_after_ids(
                document
                    .command_receipts
                    .iter()
                    .map(|receipt| receipt.id.as_str()),
            ));
    document.next_external_evidence_receipt_sequence = document
        .next_external_evidence_receipt_sequence
        .max(next_sequence_after_ids(
            document
                .external_evidence
                .iter()
                .map(|receipt| receipt.id.as_str()),
        ));
    let (duplicate_edit_indices, _) = duplicate_receipt_indices(
        document
            .edit_receipts
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_edit_indices {
        let id = next_receipt_id("edit", &mut document.next_edit_receipt_sequence);
        document.edit_receipts[index].id = id;
    }
    let (duplicate_command_indices, _) = duplicate_receipt_indices(
        document
            .command_receipts
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_command_indices {
        let id = next_receipt_id("command", &mut document.next_command_receipt_sequence);
        document.command_receipts[index].id = id;
    }
    let (duplicate_external_indices, _) = duplicate_receipt_indices(
        document
            .external_evidence
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_external_indices {
        let id = next_receipt_id(
            "external-evidence",
            &mut document.next_external_evidence_receipt_sequence,
        );
        document.external_evidence[index].id = id;
    }
    let owned_file_paths = task_owned_file_paths(document);
    document
        .latest_file_hashes
        .retain(|path, _| owned_file_paths.contains(path));
    let mut used_ids = BTreeSet::new();
    let mut duplicate_step_ids = BTreeSet::new();
    for (index, step) in document.plan.iter_mut().enumerate() {
        if !used_ids.insert(step.id.clone()) {
            duplicate_step_ids.insert(step.id.clone());
            step.id = unique_step_id(&step.id, index, &mut used_ids);
            if step.status == StepStatus::Passed {
                step.status = StepStatus::Implemented;
            }
        }
    }
    if legacy_completion_model {
        for step in &mut document.plan {
            if matches!(step.status, StepStatus::Passed | StepStatus::Completed) {
                step.status = StepStatus::Implemented;
            }
        }
        document.completion = None;
        document.risks.retain(|risk| {
            matches!(
                risk.source.as_str(),
                "edit"
                    | "command"
                    | "freshness"
                    | "plan"
                    | "plan_structure"
                    | "task_evidence_storage"
            )
        });
        document.latest_generated_artifact_hashes.clear();
    }
    rebuild_declared_requirements_and_risks(document);
    sync_plan_structure_state(document, &duplicate_step_ids);
    if document.completion_review_v2.is_none() {
        document.completion_review_v2 = Some(new_completion_review_ledger(&document.thread_id));
    }
    if source_schema_version == FROZEN_TASK_EVIDENCE_V4_SCHEMA_VERSION
        && document
            .completion
            .as_ref()
            .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
    {
        seed_migrated_v4_terminal_lineage(document);
    }
    if source_schema_version <= FROZEN_TASK_EVIDENCE_V5_SCHEMA_VERSION {
        document.source_classification_cache.clear();
    } else {
        document.source_classification_cache = canonical_source_classification_cache(
            std::mem::take(&mut document.source_classification_cache),
        );
    }
    if source_schema_version <= FROZEN_TASK_EVIDENCE_V6_SCHEMA_VERSION
        && let Some(ledger) = document.completion_review_v2.as_mut()
    {
        let requirement =
            CompletionReviewRequirement::from_obligation_mode(&ledger.obligation.mode);
        for receipt in &mut ledger.receipts {
            let (disposition, attempted_outcome) = completion_review_attempt_dimensions(
                receipt.attempt_kind,
                &receipt.infrastructure_outcome,
                receipt.review_clean,
                !receipt.findings.is_empty(),
            );
            receipt.requirement = requirement;
            receipt.disposition = disposition;
            receipt.attempted_outcome = attempted_outcome;
        }
    }
    document.schema_version = TASK_EVIDENCE_SCHEMA_VERSION;
}

fn seed_migrated_v4_terminal_lineage(document: &mut TaskEvidenceDocument) {
    let Some(mut ledger) = document.completion_review_v2.take() else {
        return;
    };
    if !ledger.receipts.is_empty() || ledger.active_review_cycle.is_some() {
        document.completion_review_v2 = Some(ledger);
        return;
    }

    let recorded_at = document.updated_at.clone();
    let source_ledger_hash = canonical_hash(
        USER_SOURCE_LEDGER_CANONICAL_FORMAT,
        &serde_json::json!({
            "rootTaskId": ledger.root_task_id,
            "completionEpoch": ledger.completion_epoch,
            "manifestRevision": ledger.manifest_revision,
            "sources": [],
            "mappings": {},
            "sourceCaptureFailed": false,
        }),
    );
    let manifest_hash = requirement_manifest_hash(ledger.manifest_revision, &[]);
    let implementation_identity_hash = canonical_hash(
        IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT,
        &serde_json::json!({
            "migration": "v4_terminal_passed",
            "rootTaskId": ledger.root_task_id,
            "completionEpoch": ledger.completion_epoch,
            "manifestRevision": ledger.manifest_revision,
            "userSourceLedgerHash": source_ledger_hash,
            "requirementManifestHash": manifest_hash,
            "mutationRevision": document.host_mutation_revision,
        }),
    );
    let dossier_snapshot_id = canonical_hash(
        DOSSIER_SNAPSHOT_CANONICAL_FORMAT,
        &serde_json::json!({
            "migration": "v4_terminal_passed",
            "implementationIdentity": implementation_identity_hash,
            "legacyCompletion": document.completion,
        }),
    );
    let initial_review_id = "review-1-0-migrated-v4-initial".to_string();
    let terminal_review_id = "review-1-0-migrated-v4-terminal".to_string();
    let receipt = |review_id: String,
                   attempt_kind: CompletionReviewAttemptKind,
                   parent_review_id: Option<String>,
                   terminal_outcome: Option<String>| CompletionReviewReceiptV2 {
        review_id,
        attempt_kind,
        parent_review_id,
        superseded_review_id: None,
        candidate_mutation_revision: document.host_mutation_revision,
        candidate_hash: implementation_identity_hash.clone(),
        implementation_identity_hash: implementation_identity_hash.clone(),
        dossier_snapshot_id: dossier_snapshot_id.clone(),
        user_source_ledger_hash: source_ledger_hash.clone(),
        requirement_manifest_hash: manifest_hash.clone(),
        attempt_identity: "migrated_v4".to_string(),
        reviewer_contract_hash: "migrated_v4".to_string(),
        findings: Vec::new(),
        dispositions: Vec::new(),
        manifest_gaps: Vec::new(),
        repair_instruction_hash: None,
        repair_baseline: None,
        baseline_hash: None,
        input_mode: None,
        delta_hash: None,
        rereview_delta: None,
        fallback_reasons: Vec::new(),
        candidate_implementation_identity: None,
        rereview_audit_hash: None,
        requirement: CompletionReviewRequirement::Supplemental,
        disposition: if attempt_kind == CompletionReviewAttemptKind::TerminalClosure {
            CompletionReviewDisposition::NotApplicable
        } else {
            CompletionReviewDisposition::Attempted
        },
        attempted_outcome: (attempt_kind == CompletionReviewAttemptKind::InitialReview)
            .then_some(CompletionReviewAttemptedOutcome::Clean),
        infrastructure_outcome: "migrated_v4_terminal".to_string(),
        review_clean: true,
        terminal_outcome,
        recorded_at: recorded_at.clone(),
    };
    ledger.receipts.push(receipt(
        initial_review_id.clone(),
        CompletionReviewAttemptKind::InitialReview,
        None,
        None,
    ));
    ledger.receipts.push(receipt(
        terminal_review_id.clone(),
        CompletionReviewAttemptKind::TerminalClosure,
        Some(initial_review_id.clone()),
        Some("passed".to_string()),
    ));
    ledger.active_review_cycle = Some(CompletionReviewCycle {
        cycle_id: "cycle-1-0-migrated-v4".to_string(),
        manifest_revision: ledger.manifest_revision,
        parent_terminal_review_id: None,
        superseded_review_id: None,
        phase: CompletionReviewCyclePhase::Closed,
        correction_consumed: false,
        manifest_gap_reconstructed: false,
        accepted_review_id: Some(initial_review_id),
        accepted_dossier_snapshot_id: Some(dossier_snapshot_id),
    });
    ledger.review_risk = CompletionReviewRisk {
        unresolved: false,
        cycle_id: Some("cycle-1-0-migrated-v4".to_string()),
        opened_at: Some(recorded_at.clone()),
        resolved_at: Some(recorded_at),
    };
    ledger.last_terminal_closure = Some(terminal_review_id);
    ledger.next_review_sequence = 3;
    document.completion_review_v2 = Some(ledger);
}

const fn initial_receipt_sequence() -> u64 {
    1
}

fn next_sequence_after_ids<'a>(ids: impl Iterator<Item = &'a str>) -> u64 {
    ids.filter_map(|id| id.rsplit_once('-')?.1.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(initial_receipt_sequence())
}

fn next_receipt_id(prefix: &str, sequence: &mut u64) -> String {
    let current = (*sequence).max(initial_receipt_sequence());
    *sequence = current.saturating_add(1);
    format!("{prefix}-{current}")
}

fn duplicate_receipt_indices<'a>(
    ids: impl Iterator<Item = (usize, &'a str)>,
) -> (Vec<usize>, BTreeSet<String>) {
    let mut seen = BTreeSet::new();
    let mut duplicate_indices = Vec::new();
    let mut duplicate_ids = BTreeSet::new();
    for (index, id) in ids {
        if !seen.insert(id.to_string()) {
            duplicate_indices.push(index);
            duplicate_ids.insert(id.to_string());
        }
    }
    (duplicate_indices, duplicate_ids)
}

fn atomic_write_evidence(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("task-evidence path {} has no parent", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file_mut().sync_all()?;
    let persisted = temp.persist(path).map_err(|err| err.error)?;
    persisted.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn find_kd4_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|candidate| candidate.join("kd4_features.toml").is_file())
        .map(Path::to_path_buf)
}

fn task_owned_file_paths(document: &TaskEvidenceDocument) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for step in &document.plan {
        paths.extend(step.edit_paths.iter().cloned());
    }
    for intent in &document.edit_intents {
        paths.extend(intent.files.iter().map(|file| file.path.clone()));
    }
    for receipt in &document.edit_receipts {
        paths.extend(receipt.files.iter().map(|file| file.path.clone()));
    }
    paths
}

fn effective_step_id(item: &PlanItemArg, index: usize, used_ids: &mut BTreeSet<String>) -> String {
    if let Some(id) = item.id.as_ref() {
        if used_ids.insert(id.clone()) {
            return id.clone();
        }
        return unique_step_id(id, index, used_ids);
    }
    let digest = sha1_hex(item.step.trim().as_bytes());
    let base = format!("step-{}", &digest[..12]);
    if used_ids.insert(base.clone()) {
        return base;
    }
    unique_step_id(&base, index, used_ids)
}

fn unique_step_id(base: &str, index: usize, used_ids: &mut BTreeSet<String>) -> String {
    let mut suffix = index.saturating_add(1);
    loop {
        let candidate = format!("{base}-{suffix}");
        if used_ids.insert(candidate.clone()) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn normalize_requested_status(requested: &StepStatus) -> StepStatus {
    match requested {
        StepStatus::Completed => StepStatus::Passed,
        status => status.clone(),
    }
}

fn step_internal_structure_matches(left: &EvidencePlanStep, right: &EvidencePlanStep) -> bool {
    left.source_owner == right.source_owner
        && left.implementation_surfaces == right.implementation_surfaces
        && left
            .mutation_obligations
            .iter()
            .map(|obligation| (&obligation.id, &obligation.description, &obligation.paths))
            .eq(right
                .mutation_obligations
                .iter()
                .map(|obligation| (&obligation.id, &obligation.description, &obligation.paths)))
        && left.validation_disposition == right.validation_disposition
        && left.external_validation_route == right.external_validation_route
}

fn implementation_obligations_satisfied(obligations: &[MutationObligationState]) -> bool {
    !obligations.is_empty() && obligations.iter().all(|obligation| obligation.satisfied)
}

fn has_unfinished_mutation_obligation(document: &TaskEvidenceDocument) -> bool {
    let active_plan_obligation = document.plan.iter().any(|step| {
        !matches!(
            step.status,
            StepStatus::Passed | StepStatus::Skipped | StepStatus::Blocked | StepStatus::Completed
        ) && step
            .mutation_obligations
            .iter()
            .any(|obligation| !obligation.satisfied)
    });
    active_plan_obligation
        || document
            .planning
            .work_unit
            .as_ref()
            .is_some_and(|work_unit| {
                work_unit
                    .mutation_obligations
                    .iter()
                    .any(|obligation| !obligation.satisfied)
            })
}

fn record_obligation_progress(
    obligations: &mut [MutationObligationState],
    changed_paths: &BTreeSet<String>,
) {
    for obligation in obligations {
        if obligation.paths.is_empty() {
            obligation.satisfied = !changed_paths.is_empty();
            obligation
                .satisfied_paths
                .extend(changed_paths.iter().cloned());
            continue;
        }
        for changed in changed_paths {
            if obligation
                .paths
                .iter()
                .any(|expected| validation_paths_overlap(expected, changed))
            {
                obligation.satisfied_paths.insert(changed.clone());
            }
        }
        obligation.satisfied = obligation.paths.iter().all(|expected| {
            obligation
                .satisfied_paths
                .iter()
                .any(|changed| validation_paths_overlap(expected, changed))
        });
    }
}

fn admissible_requested_status(step: &EvidencePlanStep) -> StepStatus {
    match step.status {
        StepStatus::Passed => match step.validation_disposition {
            ValidationDisposition::NotRequired => StepStatus::Passed,
            ValidationDisposition::UnavailableBlocked => StepStatus::Blocked,
            ValidationDisposition::Executable | ValidationDisposition::UnresolvedDiscoverable => {
                if step.validation_receipt_id.is_some() {
                    StepStatus::Passed
                } else if implementation_obligations_satisfied(&step.mutation_obligations) {
                    StepStatus::Implemented
                } else {
                    StepStatus::InProgress
                }
            }
        },
        StepStatus::Implemented
            if !step.mutation_obligations.is_empty()
                && !implementation_obligations_satisfied(&step.mutation_obligations) =>
        {
            StepStatus::InProgress
        }
        ref status => status.clone(),
    }
}

fn ensure_focused_work_unit(document: &mut TaskEvidenceDocument) -> &mut FocusedWorkUnit {
    let mut hasher = Sha256::new();
    hasher.update(b"KD4_FOCUSED_WORK_UNIT_V1\0");
    hasher.update(document.thread_id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    document.planning.work_unit.get_or_insert(FocusedWorkUnit {
        id: format!("work-{}", &digest[..24]),
        source_owner: None,
        implementation_surfaces: Vec::new(),
        acceptance_criteria: Vec::new(),
        mutation_obligations: Vec::new(),
        validation_disposition: ValidationDisposition::NotRequired,
        validation_route: None,
        external_validation_route: None,
        validation_receipt_id: None,
    })
}

fn current_action_attribution(
    document: &mut TaskEvidenceDocument,
    kind: &str,
    action_id: &str,
) -> (
    Option<String>,
    Option<u64>,
    Option<String>,
    ActionAttributionKind,
) {
    if let Some(step_id) = document.active_step_id.as_ref()
        && let Some(step) = document.plan.iter().find(|step| &step.id == step_id)
    {
        return (
            Some(step.id.clone()),
            Some(step.revision),
            None,
            ActionAttributionKind::PlannedStep,
        );
    }
    if document.plan.is_empty() {
        let work_unit_id = ensure_focused_work_unit(document).id.clone();
        return (
            None,
            None,
            Some(work_unit_id),
            ActionAttributionKind::FocusedWorkUnit,
        );
    }
    document
        .planning
        .outside_plan_actions
        .push(OutsidePlanAction {
            kind: kind.to_string(),
            action_id: action_id.to_string(),
            recorded_at: timestamp(),
        });
    trim_to_last(
        &mut document.planning.outside_plan_actions,
        MAX_OUTSIDE_PLAN_ACTIONS,
    );
    document.planning.counters.outside_plan_actions = document
        .planning
        .counters
        .outside_plan_actions
        .saturating_add(1);
    (None, None, None, ActionAttributionKind::OutsidePlan)
}

fn command_receipt_has_current_proof_identity(
    document: &TaskEvidenceDocument,
    receipt: &CommandReceipt,
) -> bool {
    let Some(validation) = receipt.validation_result.as_ref() else {
        return false;
    };
    let Some(implementation_identity) = receipt.implementation_identity_hash.as_deref() else {
        return false;
    };
    let has_current_global_identity = completion_contract_hashes(document, false).is_some_and(
        |(manifest_revision, source_hash, manifest_hash)| {
            receipt.epoch == document.evidence_epoch
                && receipt.host_mutation_revision == Some(document.host_mutation_revision)
                && receipt.manifest_revision == Some(manifest_revision)
                && receipt.user_source_ledger_hash.as_deref() == Some(source_hash.as_str())
                && receipt.requirement_manifest_hash.as_deref() == Some(manifest_hash.as_str())
        },
    );
    receipt.exit_code == 0
        && !receipt.timed_out
        && !receipt.possible_mutation
        && (has_current_global_identity
            || command_receipt_is_retained_by_path_scoped_proof(document, receipt))
        && validation.status.is_success()
        && validation.proof_key.validation_contract_version
            == codex_protocol::validation::VALIDATION_CONTRACT_VERSION
        && validation.proof_key.implementation_identity == implementation_identity
        && repository_roots_match(
            Path::new(&validation.proof_key.repository),
            Path::new(&document.start.repository_root),
        )
        && recorded_path_uri_matches(&receipt.cwd, Path::new(&validation.proof_key.cwd))
        && recorded_path_uri_matches(&receipt.cwd, Path::new(&document.start.repository_root))
}

fn command_receipt_is_retained_by_path_scoped_proof(
    document: &TaskEvidenceDocument,
    receipt: &CommandReceipt,
) -> bool {
    let retained_by_step = receipt
        .step_id
        .as_deref()
        .zip(receipt.step_revision)
        .is_some_and(|(step_id, revision)| {
            document.plan.iter().any(|step| {
                step.id == step_id
                    && step.revision == revision
                    && step.status == StepStatus::Passed
                    && step.validation_receipt_id.is_some()
                    && step.validation_route.as_ref().is_some_and(|route| {
                        route.leaves.iter().any(|leaf| {
                            crate::validation_admission::validation_argv_semantically_covers(
                                &receipt.command,
                                &leaf.argv,
                            )
                        })
                    })
            })
        });
    retained_by_step
        || receipt.work_unit_id.as_deref().is_some_and(|work_unit_id| {
            document
                .planning
                .work_unit
                .as_ref()
                .is_some_and(|work_unit| {
                    work_unit.id == work_unit_id
                        && work_unit.validation_receipt_id.is_some()
                        && work_unit.validation_route.as_ref().is_some_and(|route| {
                            route.leaves.iter().any(|leaf| {
                                crate::validation_admission::validation_argv_semantically_covers(
                                    &receipt.command,
                                    &leaf.argv,
                                )
                            })
                        })
                })
        })
}

fn accept_matching_command_proof(document: &mut TaskEvidenceDocument, receipt: &CommandReceipt) {
    if !command_receipt_has_current_proof_identity(document, receipt) {
        return;
    }
    let completed_step = receipt
        .step_id
        .as_deref()
        .zip(receipt.step_revision)
        .and_then(|(step_id, step_revision)| {
            let step = document
                .plan
                .iter()
                .find(|step| step.id == step_id && step.revision == step_revision)?;
            let route = step.validation_route.as_ref()?;
            (step.validation_disposition == ValidationDisposition::Executable
                && (step.mutation_obligations.is_empty()
                    || implementation_obligations_satisfied(&step.mutation_obligations))
                && validation_route_has_current_command_proofs(
                    document,
                    receipt,
                    route,
                    Some((step_id, step_revision)),
                    None,
                ))
            .then_some(step_id.to_string())
        });
    if let Some(step_id) = completed_step
        && let Some(step) = document.plan.iter_mut().find(|step| step.id == step_id)
    {
        step.validation_receipt_id = Some(receipt.id.clone());
        step.status = StepStatus::Passed;
        return;
    }

    let completed_work_unit = receipt.work_unit_id.as_deref().and_then(|work_unit_id| {
        let work_unit = document.planning.work_unit.as_ref()?;
        let route = work_unit.validation_route.as_ref()?;
        (work_unit.id == work_unit_id
            && work_unit.validation_disposition == ValidationDisposition::Executable
            && (work_unit.mutation_obligations.is_empty()
                || implementation_obligations_satisfied(&work_unit.mutation_obligations))
            && validation_route_has_current_command_proofs(
                document,
                receipt,
                route,
                None,
                Some(work_unit_id),
            ))
        .then_some(work_unit_id.to_string())
    });
    if let Some(work_unit_id) = completed_work_unit
        && let Some(work_unit) = document.planning.work_unit.as_mut()
        && work_unit.id == work_unit_id
    {
        work_unit.validation_receipt_id = Some(receipt.id.clone());
    }
}

fn validation_route_has_current_command_proofs(
    document: &TaskEvidenceDocument,
    current_receipt: &CommandReceipt,
    route: &ValidationRoute,
    step: Option<(&str, u64)>,
    work_unit_id: Option<&str>,
) -> bool {
    route.leaves.iter().all(|leaf| {
        let leaf_route = ValidationRoute {
            leaves: vec![leaf.clone()],
            ordering: route.ordering,
        };
        document
            .command_receipts
            .iter()
            .chain(std::iter::once(current_receipt))
            .any(|candidate| {
                let attribution_matches = match (step, work_unit_id) {
                    (Some((step_id, step_revision)), None) => {
                        candidate.step_id.as_deref() == Some(step_id)
                            && candidate.step_revision == Some(step_revision)
                    }
                    (None, Some(work_unit_id)) => {
                        candidate.work_unit_id.as_deref() == Some(work_unit_id)
                    }
                    _ => false,
                };
                attribution_matches
                    && crate::validation_admission::validation_argv_semantically_covers(
                        &candidate.command,
                        &leaf.argv,
                    )
                    && command_receipt_has_current_proof_identity(document, candidate)
                    && candidate
                        .validation_result
                        .as_ref()
                        .is_some_and(|validation| {
                            validation.route == leaf_route
                                || validation.route.leaves.iter().any(|executed_leaf| {
                                    crate::validation_admission::validation_argv_semantically_covers(
                                        &executed_leaf.argv,
                                        &leaf.argv,
                                    )
                                })
                        })
            })
    })
}

fn accept_matching_external_proof(
    document: &mut TaskEvidenceDocument,
    receipt: &ExternalEvidenceReceipt,
) {
    let fresh = receipt.tool_success
        && receipt.payload_completeness == EvidenceCompleteness::Complete
        && !receipt.truncated
        && !receipt.approximate
        && receipt.task_epoch == document.evidence_epoch
        && receipt.host_mutation_revision == Some(document.host_mutation_revision)
        && receipt.implementation_identity_hash.is_some()
        && receipt.workspace_root_fingerprint == workspace_root_fingerprint(&document.start);
    if !fresh {
        return;
    }
    let route_matches = |route: &ExternalValidationRouteInput| {
        route.server_name == receipt.server_name && route.tool_name == receipt.tool_name
    };
    if let (Some(step_id), Some(step_revision)) =
        (receipt.step_id.as_deref(), receipt.step_revision)
        && let Some(step) = document
            .plan
            .iter_mut()
            .find(|step| step.id == step_id && step.revision == step_revision)
        && step.validation_disposition == ValidationDisposition::Executable
        && step
            .external_validation_route
            .as_ref()
            .is_some_and(route_matches)
        && (step.mutation_obligations.is_empty()
            || implementation_obligations_satisfied(&step.mutation_obligations))
    {
        step.validation_receipt_id = Some(receipt.id.clone());
        step.status = StepStatus::Passed;
        return;
    }
    if let Some(work_unit_id) = receipt.work_unit_id.as_deref()
        && let Some(work_unit) = document.planning.work_unit.as_mut()
        && work_unit.id == work_unit_id
        && work_unit.validation_disposition == ValidationDisposition::Executable
        && work_unit
            .external_validation_route
            .as_ref()
            .is_some_and(route_matches)
        && (work_unit.mutation_obligations.is_empty()
            || implementation_obligations_satisfied(&work_unit.mutation_obligations))
    {
        work_unit.validation_receipt_id = Some(receipt.id.clone());
    }
}

fn step_materially_matches_item(step: &EvidencePlanStep, item: &PlanItemArg) -> bool {
    step.step == item.step
        && step.depends_on == item.depends_on
        && step.acceptance_criteria == item.acceptance_criteria
        && step.runtime_paths == item.runtime_paths
        && step.generated_artifacts == item.generated_artifacts
        && step.risks == item.risks
        && step.requires_desktop_activation == item.requires_desktop_activation
        && (item.validation_route.is_none() || step.validation_route == item.validation_route)
}

fn plan_item_from_evidence(step: &EvidencePlanStep) -> PlanItemArg {
    PlanItemArg {
        id: Some(step.id.clone()),
        step: step.step.clone(),
        status: step.status.clone(),
        depends_on: step.depends_on.clone(),
        acceptance_criteria: step.acceptance_criteria.clone(),
        runtime_paths: step.runtime_paths.clone(),
        generated_artifacts: step.generated_artifacts.clone(),
        risks: step.risks.clone(),
        requires_desktop_activation: step.requires_desktop_activation,
        validation_route: step.validation_route.clone(),
    }
}

fn validation_route_covered_paths(route: &ValidationRoute) -> BTreeSet<String> {
    route
        .leaves
        .iter()
        .flat_map(validation_leaf_covered_paths)
        .collect()
}

fn validation_leaf_covered_paths(
    leaf: &codex_protocol::plan_tool::ValidationRouteLeaf,
) -> BTreeSet<String> {
    leaf.covered_paths
        .iter()
        .map(|path| path.replace('\\', "/").trim_start_matches("./").to_string())
        .collect()
}

fn validation_leaf_implementation_identity(
    implementation_revision: u64,
    leaf: &codex_protocol::plan_tool::ValidationRouteLeaf,
    covered_manifest: &[FileHashSnapshot],
) -> String {
    let covered_paths = validation_leaf_covered_paths(leaf);
    let repository_wide = covered_paths.is_empty();
    let leaf_manifest = covered_manifest
        .iter()
        .filter(|snapshot| {
            let snapshot_path = snapshot.path.replace('\\', "/");
            covered_paths
                .iter()
                .any(|covered| validation_paths_overlap(covered, &snapshot_path))
        })
        .cloned()
        .collect::<Vec<_>>();
    validation_implementation_identity(implementation_revision, repository_wide, &leaf_manifest)
}

fn validation_implementation_identity(
    implementation_revision: u64,
    repository_wide: bool,
    covered_manifest: &[FileHashSnapshot],
) -> String {
    if repository_wide {
        return format!("repository-revision:{implementation_revision}");
    }
    let canonical = serde_json::to_vec(&(
        "KD4_VALIDATION_IMPLEMENTATION_IDENTITY_V1",
        covered_manifest,
    ))
    .unwrap_or_default();
    format!("covered-manifest:{:x}", Sha256::digest(canonical))
}

fn validation_paths_overlap(left: &str, right: &str) -> bool {
    let left = left.trim_end_matches('/');
    let right = right.trim_end_matches('/');
    left == right
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn workspace_proof_scope(document: &TaskEvidenceDocument) -> WorkspaceProofScope {
    let mut paths = document
        .latest_file_hashes
        .keys()
        .chain(document.latest_generated_artifact_hashes.keys())
        .map(|path| normalize_slashes(path))
        .collect::<BTreeSet<_>>();
    let mut contracts = BTreeSet::new();
    for step in &document.plan {
        paths.extend(step.edit_paths.iter().map(|path| normalize_slashes(path)));
        paths.extend(
            step.runtime_paths
                .iter()
                .map(|path| normalize_slashes(path)),
        );
        paths.extend(
            step.generated_artifacts
                .iter()
                .map(|path| normalize_slashes(path)),
        );
        paths.extend(
            step.implementation_surfaces
                .iter()
                .map(|path| normalize_slashes(path)),
        );
        for obligation in &step.mutation_obligations {
            paths.extend(obligation.paths.iter().map(|path| normalize_slashes(path)));
        }
        if let Some(route) = step.validation_route.as_ref() {
            paths.extend(validation_route_covered_paths(route));
            contracts.extend(
                route
                    .leaves
                    .iter()
                    .flat_map(|leaf| leaf.covered_contracts.iter().cloned()),
            );
        }
    }
    if let Some(work_unit) = document.planning.work_unit.as_ref() {
        paths.extend(
            work_unit
                .implementation_surfaces
                .iter()
                .map(|path| normalize_slashes(path)),
        );
        for obligation in &work_unit.mutation_obligations {
            paths.extend(obligation.paths.iter().map(|path| normalize_slashes(path)));
        }
        if let Some(route) = work_unit.validation_route.as_ref() {
            paths.extend(validation_route_covered_paths(route));
            contracts.extend(
                route
                    .leaves
                    .iter()
                    .flat_map(|leaf| leaf.covered_contracts.iter().cloned()),
            );
        }
    }
    paths.retain(|path| !path.is_empty());
    contracts.retain(|contract| !contract.trim().is_empty());
    let baseline_epoch = document
        .completion_review_v2
        .as_ref()
        .map(|ledger| ledger.workspace_event_baseline_epoch)
        .unwrap_or_default();
    let identity = canonical_hash(
        "workspace-proof-scope-v1",
        &serde_json::json!({
            "paths": &paths,
            "contracts": &contracts,
            "proof_baseline_epoch": baseline_epoch,
        }),
    );
    WorkspaceProofScope {
        identity,
        paths,
        contracts,
    }
}

fn classify_workspace_event(
    event: &TaskAttributedWorkspaceEvent,
    scope: &WorkspaceProofScope,
) -> WorkspaceEventRelevance {
    let repository_wide = event
        .paths
        .iter()
        .any(|path| path == codex_agent_task_store::REPOSITORY_WIDE_PATH);
    let path_overlap = !repository_wide
        && event.paths.iter().any(|event_path| {
            scope.paths.iter().any(|controlled_path| {
                validation_paths_overlap(controlled_path, &normalize_slashes(event_path))
            })
        });
    let contract_overlap = event
        .contracts
        .iter()
        .any(|contract| scope.contracts.contains(contract));
    if path_overlap || contract_overlap {
        return WorkspaceEventRelevance::Relevant;
    }
    if repository_wide
        || (event.paths.is_empty() && event.contracts.is_empty())
        || event.attribution_confidence
            != Some(codex_agent_task_store::AttributionConfidence::Definitive)
    {
        return WorkspaceEventRelevance::Unknown;
    }
    WorkspaceEventRelevance::Unrelated
}

fn workspace_event_actor_is_admitted(
    event: &codex_agent_task_store::WorkspaceEvent,
    root_actor_id: &str,
    legacy_actor_prefix: &str,
    same_root_typed_actor_ids: &BTreeSet<String>,
) -> bool {
    match event.actor_kind {
        codex_agent_task_store::WorkspaceActorKind::Root => {
            event.actor_id.as_deref() == Some(root_actor_id)
        }
        codex_agent_task_store::WorkspaceActorKind::Legacy => event
            .actor_id
            .as_deref()
            .is_some_and(|actor_id| actor_id.starts_with(legacy_actor_prefix)),
        codex_agent_task_store::WorkspaceActorKind::Typed => {
            event.attribution_confidence
                == codex_agent_task_store::AttributionConfidence::Definitive
                && event
                    .actor_id
                    .as_ref()
                    .is_some_and(|actor_id| same_root_typed_actor_ids.contains(actor_id))
        }
        codex_agent_task_store::WorkspaceActorKind::External => false,
    }
}

const fn workspace_scope_history_is_unknown(scope_changed: bool, history_complete: bool) -> bool {
    scope_changed && !history_complete
}

fn implementation_dependencies_satisfied(
    document: &TaskEvidenceDocument,
    step: &EvidencePlanStep,
) -> bool {
    step.depends_on.iter().all(|dependency| {
        document
            .plan
            .iter()
            .find(|candidate| &candidate.id == dependency)
            .is_some_and(|candidate| {
                matches!(
                    candidate.status,
                    StepStatus::Implemented | StepStatus::Passed | StepStatus::Skipped
                )
            })
    })
}

fn sync_plan_structure_state(
    document: &mut TaskEvidenceDocument,
    duplicate_explicit_ids: &BTreeSet<String>,
) {
    let active_ids = document
        .plan
        .iter()
        .filter(|step| step.status == StepStatus::InProgress)
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();
    document.active_step_id = if active_ids.len() == 1 {
        active_ids.first().cloned()
    } else {
        None
    };
    if active_ids.len() > 1 {
        upsert_risk(
            document,
            EvidenceRisk {
                id: "plan-structure-multiple-active-steps".to_string(),
                description: format!(
                    "plan declares multiple in-progress steps: {}",
                    active_ids.join(", ")
                ),
                source: "plan_structure".to_string(),
                blocking: true,
                resolved: false,
                epoch: document.evidence_epoch,
            },
        );
    } else {
        resolve_risk(document, "plan-structure-multiple-active-steps");
    }
    if duplicate_explicit_ids.is_empty() {
        resolve_risk(document, "plan-structure-duplicate-step-ids");
    } else {
        upsert_risk(
            document,
            EvidenceRisk {
                id: "plan-structure-duplicate-step-ids".to_string(),
                description: format!(
                    "plan contained duplicate explicit step ids: {}",
                    duplicate_explicit_ids
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                source: "plan_structure".to_string(),
                blocking: true,
                resolved: false,
                epoch: document.evidence_epoch,
            },
        );
    }
}

fn rebuild_declared_requirements_and_risks(document: &mut TaskEvidenceDocument) {
    document.generated_artifact_requirements.clear();
    document.risks.retain(|risk| risk.source != "plan");
    let mut requirements = Vec::new();
    let mut risks = Vec::new();
    for step in &document.plan {
        if step.status != StepStatus::Skipped {
            for (index, path) in step.generated_artifacts.iter().enumerate() {
                requirements.push(GeneratedArtifactRequirement {
                    id: format!("plan:{}:artifact:{index}", step.id),
                    step_id: Some(step.id.clone()),
                    path: Some(normalize_slashes(path)),
                });
            }
        }
        for (index, description) in step.risks.iter().enumerate() {
            risks.push(EvidenceRisk {
                id: format!("plan:{}:risk:{index}", step.id),
                description: description.clone(),
                source: "plan".to_string(),
                blocking: false,
                resolved: step.status == StepStatus::Passed,
                epoch: document.evidence_epoch,
            });
        }
    }
    document
        .generated_artifact_requirements
        .extend(requirements);
    let required_artifact_paths = document
        .generated_artifact_requirements
        .iter()
        .filter_map(|requirement| requirement.path.as_deref())
        .collect::<BTreeSet<_>>();
    document
        .latest_generated_artifact_hashes
        .retain(|path, _| required_artifact_paths.contains(path.as_str()));
    document.risks.extend(risks);
}

fn plan_is_terminally_acknowledged(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        && document
            .plan
            .iter()
            .all(|step| matches!(step.status, StepStatus::Passed | StepStatus::Skipped))
}

fn resolve_recoverable_runtime_risks(document: &mut TaskEvidenceDocument) {
    for risk in &mut document.risks {
        if !risk.blocking && matches!(risk.source.as_str(), "edit" | "command" | "freshness") {
            risk.resolved = true;
        }
    }
}

fn edit_outcome_succeeded(outcome: &str) -> bool {
    outcome == "completed"
}

fn generated_artifact_is_currently_available(document: &TaskEvidenceDocument, path: &str) -> bool {
    let normalized = normalize_slashes(path);
    let Some(current) = document.latest_generated_artifact_hashes.get(&normalized) else {
        return false;
    };
    current.exists && current.read_error.is_none() && current.sha1.is_some()
}

fn dependency_cycle_members(document: &TaskEvidenceDocument) -> BTreeSet<String> {
    fn visit<'a>(
        id: &'a str,
        steps: &BTreeMap<&'a str, &'a EvidencePlanStep>,
        states: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
        cycle_members: &mut BTreeSet<String>,
    ) {
        match states.get(id).copied() {
            Some(2) => return,
            Some(1) => {
                if let Some(cycle_start) = stack.iter().position(|candidate| *candidate == id) {
                    cycle_members.extend(
                        stack[cycle_start..]
                            .iter()
                            .map(|member| (*member).to_string()),
                    );
                }
                return;
            }
            _ => {}
        }
        let Some(step) = steps.get(id) else {
            return;
        };
        states.insert(id, 1);
        stack.push(id);
        for dependency in &step.depends_on {
            if steps.contains_key(dependency.as_str()) {
                visit(dependency, steps, states, stack, cycle_members);
            }
        }
        stack.pop();
        states.insert(id, 2);
    }

    let steps = document
        .plan
        .iter()
        .filter(|step| step.status != StepStatus::Skipped)
        .map(|step| (step.id.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    let mut cycle_members = BTreeSet::new();
    for id in steps.keys().copied() {
        visit(id, &steps, &mut states, &mut stack, &mut cycle_members);
    }
    cycle_members
}

fn completion_review_locally_obtainable_proof_routes(gate: &TaskCompletionGate) -> Vec<String> {
    if gate.status != TaskCompletionStatus::Partial {
        return Vec::new();
    }
    let mut routes = gate
        .reasons
        .iter()
        .filter_map(|reason| {
            if reason.starts_with("plan steps are not acknowledged as passed:")
                || (reason.starts_with("plan step `") && reason.contains("unfinished step"))
            {
                Some(format!(
                    "Complete the named durable plan obligation and attach its deterministic focused proof: {reason}"
                ))
            } else if reason
                .starts_with("required Desktop activation receipt is missing or stale")
            {
                Some(
                    "Desktop activation proof is unavailable until the native host implements a publish-ID-bound initialization handshake; command output cannot satisfy this requirement"
                        .to_string(),
                )
            } else if reason.starts_with(
                "required generated artifact is missing, unreadable, or unhashable:",
            ) {
                Some(format!(
                    "Regenerate or restore the named artifact through its owning generator, then record the focused proof: {reason}"
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    routes.sort();
    routes.dedup();
    routes
}

fn derive_completion_gate(
    document: &TaskEvidenceDocument,
    evidence_path: Option<&Path>,
    desktop_activation_runtime: &DesktopActivationRuntimeSnapshot,
) -> TaskCompletionGate {
    let mut blocked = Vec::new();
    let mut partial = Vec::new();
    let blocked_steps = document
        .plan
        .iter()
        .filter(|step| step.status == StepStatus::Blocked)
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();
    if !blocked_steps.is_empty() {
        blocked.push(format!("blocked plan steps: {}", blocked_steps.join(", ")));
    }
    let unacknowledged_steps = document
        .plan
        .iter()
        .filter(|step| !matches!(step.status, StepStatus::Passed | StepStatus::Skipped))
        .map(|step| format!("{} ({:?})", step.id, step.status))
        .collect::<Vec<_>>();
    if !unacknowledged_steps.is_empty() {
        partial.push(format!(
            "plan steps are not acknowledged as passed: {}",
            unacknowledged_steps.join(", ")
        ));
    }
    let steps_by_id = document
        .plan
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    for step in document
        .plan
        .iter()
        .filter(|step| step.status != StepStatus::Skipped)
    {
        for dependency in &step.depends_on {
            if dependency == &step.id {
                blocked.push(format!("plan step `{}` cannot depend on itself", step.id));
                continue;
            }
            let Some(dependency_step) = steps_by_id.get(dependency.as_str()) else {
                blocked.push(format!(
                    "plan step `{}` depends on missing step `{dependency}`",
                    step.id
                ));
                continue;
            };
            if !matches!(
                dependency_step.status,
                StepStatus::Passed | StepStatus::Skipped
            ) {
                partial.push(format!(
                    "plan step `{}` depends on unfinished step `{dependency}` ({:?})",
                    step.id, dependency_step.status
                ));
            }
        }
    }
    let cycle_members = dependency_cycle_members(document);
    if !cycle_members.is_empty() {
        blocked.push(format!(
            "plan dependency cycle includes: {}",
            cycle_members.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if document
        .plan
        .iter()
        .any(|step| step.status != StepStatus::Skipped && step.requires_desktop_activation)
        && document
            .desktop_activation_receipt
            .as_ref()
            .is_none_or(|receipt| {
                receipt.epoch != document.evidence_epoch
                    || !desktop_activation_receipt_is_complete(
                        receipt,
                        desktop_activation_runtime,
                        document,
                    )
            })
    {
        partial.push(match desktop_activation_runtime.availability {
            DesktopInstallEvidenceAvailability::NoAuthenticatedHostTransport => {
                "required Desktop activation receipt is missing or stale: no authenticated host transport"
                    .to_string()
            }
            DesktopInstallEvidenceAvailability::AuthenticatedHostBootstrap => {
                "required Desktop activation receipt is missing or stale".to_string()
            }
        });
    }
    for requirement in &document.generated_artifact_requirements {
        if let Some(path) = requirement.path.as_ref()
            && !generated_artifact_is_currently_available(document, path)
        {
            partial.push(format!(
                "required generated artifact is missing, unreadable, or unhashable: {path}"
            ));
        }
    }
    for snapshot in document
        .latest_file_hashes
        .values()
        .filter(|snapshot| snapshot.read_error.is_some())
    {
        partial.push(format!(
            "task-controlled file is unreadable and cannot be freshness-checked: {}",
            snapshot.path
        ));
    }
    blocked.extend(
        document
            .risks
            .iter()
            .filter(|risk| !risk.resolved && risk.blocking)
            .map(|risk| risk.description.clone()),
    );
    partial.extend(
        document
            .risks
            .iter()
            .filter(|risk| !risk.resolved && !risk.blocking)
            .map(|risk| risk.description.clone()),
    );
    blocked.sort();
    blocked.dedup();
    partial.sort();
    partial.dedup();
    let (status, reasons) = if !blocked.is_empty() {
        blocked.extend(partial);
        (TaskCompletionStatus::Blocked, blocked)
    } else if !partial.is_empty() {
        (TaskCompletionStatus::Partial, partial)
    } else {
        (TaskCompletionStatus::Passed, Vec::new())
    };
    TaskCompletionGate {
        status,
        reasons,
        evidence_path: evidence_path.map(|path| path.to_string_lossy().into_owned()),
    }
}

fn overlay_completion_review_gate(
    document: &TaskEvidenceDocument,
    gate: &mut TaskCompletionGate,
    source_capture_failed: bool,
) {
    let Some(ledger) = document.completion_review_v2.as_ref() else {
        return;
    };
    let cycle_phase = ledger.active_review_cycle.as_ref().map(|cycle| cycle.phase);
    let blocked = cycle_phase == Some(CompletionReviewCyclePhase::TerminalBlocked);
    let mandatory = CompletionReviewRequirement::from_obligation_mode(&ledger.obligation.mode)
        == CompletionReviewRequirement::Mandatory;
    let current_clean_attempt = ledger.receipts.iter().rev().any(|receipt| {
        receipt.candidate_mutation_revision == document.host_mutation_revision
            && receipt.disposition == CompletionReviewDisposition::Attempted
            && receipt.attempted_outcome == Some(CompletionReviewAttemptedOutcome::Clean)
    });
    let mandatory_proof_missing = mandatory
        && ledger
            .obligation
            .required_attempt_identity
            .as_deref()
            .map_or(!current_clean_attempt, |required| {
                ledger.obligation.satisfied_attempt_identity.as_deref() != Some(required)
            });
    let actionable_findings_pending = matches!(
        cycle_phase,
        Some(
            CompletionReviewCyclePhase::CorrectionPending
                | CompletionReviewCyclePhase::RereviewPending
                | CompletionReviewCyclePhase::TerminalPartial
        )
    ) && ledger.receipts.iter().rev().any(|receipt| {
        receipt.candidate_mutation_revision == document.host_mutation_revision
            && receipt.disposition == CompletionReviewDisposition::Attempted
            && receipt.attempted_outcome
                == Some(CompletionReviewAttemptedOutcome::ActionableFindings)
    });
    let unresolved = source_capture_failed
        || ledger.review_risk.unresolved
        || cycle_phase.is_some_and(|phase| phase != CompletionReviewCyclePhase::Closed)
        || mandatory_proof_missing;
    if !mandatory && !source_capture_failed && !actionable_findings_pending {
        return;
    }
    if !unresolved && !blocked {
        return;
    }
    if source_capture_failed {
        gate.reasons.push(
            "a user source could not be durably captured before completion review".to_string(),
        );
    }
    if blocked {
        gate.reasons
            .push("completion review is blocked by an external impediment".to_string());
        gate.status = TaskCompletionStatus::Blocked;
    } else if gate.status != TaskCompletionStatus::Blocked {
        if mandatory_proof_missing {
            gate.reasons.push(
                "mandatory completion-review proof is missing for the current candidate"
                    .to_string(),
            );
        } else {
            gate.reasons.push(
                "completion review risk remains unresolved for the current candidate".to_string(),
            );
        }
        gate.status = TaskCompletionStatus::Partial;
    }
    gate.reasons.sort();
    gate.reasons.dedup();
}

fn invalidate_for_mutation(
    document: &mut TaskEvidenceDocument,
    affected_paths: Option<&BTreeSet<String>>,
) {
    for fact in document.planning.facts.values_mut() {
        if !fact.dependencies_current {
            continue;
        }
        let affected = fact.depends_on_paths.is_empty()
            || affected_paths.is_none_or(|paths| {
                paths.iter().any(|changed| {
                    fact.depends_on_paths
                        .iter()
                        .any(|dependency| validation_paths_overlap(dependency, changed))
                })
            });
        if affected {
            fact.dependencies_current = false;
        }
    }
    let repair_count_by_lineage = std::mem::take(&mut document.final_proof.repair_count_by_lineage);
    document.final_proof = FinalProofStateV1 {
        repair_count_by_lineage,
        ..FinalProofStateV1::default()
    };
    let acknowledgement_invalidated =
        document
            .batch_acknowledgement
            .as_ref()
            .is_some_and(|acknowledgement| {
                document
                    .plan
                    .iter()
                    .find(|step| step.id == acknowledgement.step_id)
                    .is_none_or(|step| mutation_can_affect_step(step, affected_paths))
            });
    if acknowledgement_invalidated {
        document.batch_acknowledgement = None;
    }
    document.host_mutation_revision = document.host_mutation_revision.saturating_add(1);
    if let Some(acknowledgement) = document.batch_acknowledgement.as_mut() {
        // A mutation proven disjoint from the route does not stale the explicit
        // implementation boundary. Rebind only its orchestration revision; the
        // semantic proof identity remains the covered manifest hash.
        acknowledgement.implementation_revision = document.host_mutation_revision;
    }
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.last_mutation_at = Some(timestamp());
    document.desktop_activation_receipt = None;
    document.completion = None;
    if let Some(ledger) = document.completion_review_v2.as_mut()
        && let Some(cycle) = ledger.active_review_cycle.as_mut()
    {
        match cycle.phase {
            CompletionReviewCyclePhase::RereviewPending if cycle.correction_consumed => {
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
            }
            CompletionReviewCyclePhase::ProvisionalClean => {
                cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
                ledger.review_risk.unresolved = true;
                ledger.review_risk.resolved_at = None;
            }
            CompletionReviewCyclePhase::TerminalPartial
            | CompletionReviewCyclePhase::TerminalBlocked => {
                let parent_terminal_review_id = ledger
                    .receipts
                    .iter()
                    .rev()
                    .find(|receipt| {
                        receipt.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
                    })
                    .map(|receipt| receipt.review_id.clone());
                *cycle = CompletionReviewCycle {
                    cycle_id: format!(
                        "cycle-{}-{}-resume-{}",
                        ledger.completion_epoch,
                        ledger.manifest_revision,
                        document.host_mutation_revision
                    ),
                    manifest_revision: ledger.manifest_revision,
                    parent_terminal_review_id,
                    superseded_review_id: None,
                    phase: CompletionReviewCyclePhase::InitialReviewPending,
                    correction_consumed: false,
                    manifest_gap_reconstructed: false,
                    accepted_review_id: None,
                    accepted_dossier_snapshot_id: None,
                };
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = Some(cycle.cycle_id.clone());
                ledger.review_risk.opened_at = Some(timestamp());
                ledger.review_risk.resolved_at = None;
            }
            CompletionReviewCyclePhase::ClassificationPending
            | CompletionReviewCyclePhase::InitialReviewPending
            | CompletionReviewCyclePhase::CorrectionPending
            | CompletionReviewCyclePhase::RereviewPending
            | CompletionReviewCyclePhase::Closed => {}
        }
    }
    for step in &mut document.plan {
        if step.status == StepStatus::Passed && mutation_can_affect_step(step, affected_paths) {
            step.status = StepStatus::Implemented;
            step.validation_receipt_id = None;
        }
    }
    if let Some(work_unit) = document.planning.work_unit.as_mut()
        && mutation_can_affect_work_unit(work_unit, affected_paths)
    {
        work_unit.validation_receipt_id = None;
    }
}

fn mutation_can_affect_step(
    step: &EvidencePlanStep,
    affected_paths: Option<&BTreeSet<String>>,
) -> bool {
    let Some(affected_paths) = affected_paths else {
        return true;
    };
    if step.validation_route.as_ref().is_some_and(|route| {
        route
            .leaves
            .iter()
            .any(|leaf| leaf.covered_paths.is_empty())
    }) {
        return true;
    }
    let mut covered = step
        .implementation_surfaces
        .iter()
        .map(|path| normalize_slashes(path))
        .chain(
            step.mutation_obligations
                .iter()
                .flat_map(|obligation| obligation.paths.iter().map(|path| normalize_slashes(path))),
        )
        .chain(step.edit_paths.iter().cloned())
        .collect::<BTreeSet<_>>();
    if let Some(route) = step.validation_route.as_ref() {
        covered.extend(validation_route_covered_paths(route));
    }
    covered.is_empty()
        || affected_paths.iter().any(|changed| {
            covered
                .iter()
                .any(|expected| validation_paths_overlap(expected, changed))
        })
}

fn mutation_can_affect_work_unit(
    work_unit: &FocusedWorkUnit,
    affected_paths: Option<&BTreeSet<String>>,
) -> bool {
    let Some(affected_paths) = affected_paths else {
        return true;
    };
    if work_unit.validation_route.as_ref().is_some_and(|route| {
        route
            .leaves
            .iter()
            .any(|leaf| leaf.covered_paths.is_empty())
    }) {
        return true;
    }
    let mut covered = work_unit
        .implementation_surfaces
        .iter()
        .map(|path| normalize_slashes(path))
        .chain(
            work_unit
                .mutation_obligations
                .iter()
                .flat_map(|obligation| obligation.paths.iter().map(|path| normalize_slashes(path))),
        )
        .collect::<BTreeSet<_>>();
    if let Some(route) = work_unit.validation_route.as_ref() {
        covered.extend(validation_route_covered_paths(route));
    }
    covered.is_empty()
        || affected_paths.iter().any(|changed| {
            covered
                .iter()
                .any(|expected| validation_paths_overlap(expected, changed))
        })
}

fn invalidate_for_after_agent_mutation(document: &mut TaskEvidenceDocument) {
    let passed_steps = document
        .plan
        .iter()
        .filter(|step| step.status == StepStatus::Passed)
        .map(|step| (step.id.clone(), step.validation_receipt_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let focused_receipt = document
        .planning
        .work_unit
        .as_ref()
        .and_then(|work_unit| work_unit.validation_receipt_id.clone());
    invalidate_for_mutation(document, None);
    for step in &mut document.plan {
        if let Some(receipt_id) = passed_steps.get(&step.id)
            && step.status == StepStatus::Implemented
        {
            step.status = StepStatus::Passed;
            step.validation_receipt_id.clone_from(receipt_id);
        }
    }
    if let Some(work_unit) = document.planning.work_unit.as_mut() {
        work_unit.validation_receipt_id = focused_receipt;
    }
}

fn invalidate_for_plan_change(document: &mut TaskEvidenceDocument) {
    document.batch_acknowledgement = None;
    let repair_count_by_lineage = std::mem::take(&mut document.final_proof.repair_count_by_lineage);
    document.final_proof = FinalProofStateV1 {
        repair_count_by_lineage,
        ..FinalProofStateV1::default()
    };
    document.completion = None;
}

fn resolve_risk(document: &mut TaskEvidenceDocument, id: &str) {
    if let Some(risk) = document.risks.iter_mut().find(|risk| risk.id == id) {
        risk.resolved = true;
    }
}

fn unreadable_file_risk(path: &str, epoch: u64, source: &str) -> EvidenceRisk {
    EvidenceRisk {
        id: unreadable_file_risk_id(path),
        description: format!(
            "task-controlled file `{}` is unreadable and cannot be freshness-checked",
            normalize_slashes(path)
        ),
        source: source.to_string(),
        blocking: false,
        resolved: false,
        epoch,
    }
}

fn task_evidence_storage_risk(reason: &str, epoch: u64) -> EvidenceRisk {
    EvidenceRisk {
        id: "task-evidence-storage-failure".to_string(),
        description: format!("task-evidence storage is unavailable: {reason}"),
        source: "task_evidence_storage".to_string(),
        blocking: false,
        resolved: false,
        epoch,
    }
}

fn task_evidence_recovery_risk(reason: &str, epoch: u64) -> EvidenceRisk {
    EvidenceRisk {
        id: "completion-review-lineage-recovery-failure".to_string(),
        description: format!("completion-review lineage could not be recovered: {reason}"),
        source: "completion_review_recovery".to_string(),
        blocking: false,
        resolved: false,
        epoch,
    }
}

fn unreadable_file_risk_id(path: &str) -> String {
    let digest = sha1_hex(normalize_slashes(path).as_bytes());
    format!("unreadable-file-{}", &digest[..16])
}

fn upsert_risk(document: &mut TaskEvidenceDocument, risk: EvidenceRisk) {
    if let Some(existing) = document
        .risks
        .iter_mut()
        .find(|existing| existing.id == risk.id)
    {
        *existing = risk;
    } else {
        document.risks.push(risk);
    }
}

fn normalize_input_path(repo_root: &Path, cwd: Option<&Path>, path: &Path) -> String {
    let absolute = lexical_clean_path(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.unwrap_or(repo_root).join(path)
    });
    let repo_root = lexical_clean_path(repo_root.to_path_buf());
    absolute
        .strip_prefix(&repo_root)
        .map(Path::to_path_buf)
        .unwrap_or(absolute)
        .to_string_lossy()
        .replace('\\', "/")
}

fn lexical_absolute_path(repo_root: &Path, path: &Path) -> PathBuf {
    lexical_clean_path(if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    })
}

fn lexical_clean_path(path: PathBuf) -> PathBuf {
    use std::path::Component;

    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !cleaned.pop() {
                    cleaned.push(component.as_os_str());
                }
            }
            _ => cleaned.push(component.as_os_str()),
        }
    }
    cleaned
}

fn freshness_manifest(document: &TaskEvidenceDocument) -> FreshnessManifest {
    FreshnessManifest {
        state_signature: completion_freshness_state_signature(document),
        evidence_epoch: document.evidence_epoch,
        requirements: document.generated_artifact_requirements.clone(),
        tracked: document.latest_file_hashes.clone(),
        artifacts: document.latest_generated_artifact_hashes.clone(),
        artifact_paths: document
            .generated_artifact_requirements
            .iter()
            .filter_map(|requirement| requirement.path.clone())
            .map(|path| normalize_slashes(&path))
            .collect(),
    }
}

fn completion_freshness_state_signature(document: &TaskEvidenceDocument) -> String {
    let mut state = document.clone();
    state.revision = 0;
    state.updated_at.clear();
    state.completion = None;
    state
        .risks
        .retain(|risk| risk.id != "task-evidence-storage-failure");
    let value = serde_json::to_value(state)
        .unwrap_or_else(|_| unreachable!("task evidence state must serialize"));
    canonical_hash("KD4_FRESHNESS_PROOF_STATE_V1", &value)
}

fn validated_generated_artifact_path(
    repo_root: &Path,
    normalized: &str,
) -> Result<PathBuf, FileHashSnapshot> {
    let normalized = normalize_slashes(normalized);
    let path = Path::new(&normalized);
    if path.is_absolute() {
        return Err(rejected_generated_artifact_snapshot(
            &normalized,
            "AbsoluteArtifactPath",
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return Err(rejected_generated_artifact_snapshot(
            &normalized,
            "PathTraversalOutsideRepository",
        ));
    }
    let absolute = lexical_absolute_path(repo_root, path);
    if !generated_artifact_path_is_contained(repo_root, &absolute) {
        return Err(rejected_generated_artifact_snapshot(
            &normalized,
            "OutsideRepository",
        ));
    }
    Ok(absolute)
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

async fn snapshot_file(repo_root: &Path, normalized: &str) -> FileHashSnapshot {
    let absolute = match validated_generated_artifact_path(repo_root, normalized) {
        Ok(absolute) => absolute,
        Err(rejected) => return rejected,
    };
    match sha1_path(&absolute).await {
        Ok(sha1) => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: Some(sha1),
            exists: true,
            read_error: None,
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: None,
            exists: false,
            read_error: None,
        },
        Err(err) => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: None,
            exists: tokio::fs::symlink_metadata(&absolute).await.is_ok(),
            read_error: Some(format!("{:?}", err.kind())),
        },
    }
}

fn generated_artifact_path_is_contained(repo_root: &Path, candidate: &Path) -> bool {
    let Ok(canonical_repo_root) = dunce::canonicalize(repo_root) else {
        return false;
    };
    let mut existing_ancestor = candidate.to_path_buf();
    loop {
        match dunce::canonicalize(&existing_ancestor) {
            Ok(canonical_candidate) => {
                return canonical_candidate == canonical_repo_root
                    || canonical_candidate
                        .strip_prefix(&canonical_repo_root)
                        .is_ok();
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if !existing_ancestor.pop() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

fn rejected_generated_artifact_snapshot(normalized: &str, reason: &str) -> FileHashSnapshot {
    FileHashSnapshot {
        path: normalize_slashes(normalized),
        sha1: None,
        exists: false,
        read_error: Some(reason.to_string()),
    }
}

async fn trusted_file_token(file: &tokio::fs::File) -> Option<TrustedFileToken> {
    let metadata = file.metadata().await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    trusted_file_token_from_handle(file, &metadata)
}

#[cfg(unix)]
fn trusted_file_token_from_handle(
    _file: &tokio::fs::File,
    metadata: &std::fs::Metadata,
) -> Option<TrustedFileToken> {
    use std::os::unix::fs::MetadataExt;

    Some(TrustedFileToken {
        len: metadata.len(),
        dev: metadata.dev(),
        ino: metadata.ino(),
        mode: metadata.mode(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn trusted_file_token_from_handle(
    file: &tokio::fs::File,
    metadata: &std::fs::Metadata,
) -> Option<TrustedFileToken> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileBasicInfo;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let handle = file.as_raw_handle() as HANDLE;
    let mut id = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: `file` owns a valid handle and `id` is correctly sized writable storage.
    let id_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            id.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if id_ok == 0 {
        return None;
    }
    let mut basic = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: `file` owns a valid handle and `basic` is correctly sized writable storage.
    let basic_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            basic.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if basic_ok == 0 {
        return None;
    }
    // SAFETY: successful calls initialized both complete structures.
    let id = unsafe { id.assume_init() };
    let basic = unsafe { basic.assume_init() };
    if id.FileId.Identifier.iter().all(|byte| *byte == 0)
        || basic.LastWriteTime == 0
        || basic.ChangeTime == 0
    {
        return None;
    }
    Some(TrustedFileToken {
        len: metadata.len(),
        volume_serial_number: id.VolumeSerialNumber,
        file_id: id.FileId.Identifier,
        file_attributes: basic.FileAttributes,
        last_write_time: basic.LastWriteTime,
        change_time: basic.ChangeTime,
    })
}

#[cfg(not(any(unix, windows)))]
fn trusted_file_token_from_handle(
    _file: &tokio::fs::File,
    _metadata: &std::fs::Metadata,
) -> Option<TrustedFileToken> {
    None
}

async fn sha1_file(path: &Path) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0_u8; FILE_HASH_CHUNK_SIZE];
    loop {
        let bytes_read = file.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn sha1_path(path: &Path) -> io::Result<String> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if !metadata.is_dir() {
        return sha1_file(path).await;
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || sha1_directory(&path))
        .await
        .map_err(|error| io::Error::other(format!("directory hash task failed: {error}")))?
}

fn sha1_directory(root: &Path) -> io::Result<String> {
    use std::io::Read as _;

    let mut entries = vec![root.to_path_buf()];
    collect_directory_entries(root, &mut entries)?;
    entries.sort();

    let mut hasher = Sha1::new();
    for path in entries {
        let relative = path.strip_prefix(root).map_err(io::Error::other)?;
        let relative = normalize_slashes(&relative.to_string_lossy());
        let metadata = std::fs::symlink_metadata(&path)?;
        let kind = if metadata.file_type().is_symlink() {
            b'L'
        } else if metadata.is_dir() {
            b'D'
        } else if metadata.is_file() {
            b'F'
        } else {
            b'O'
        };
        hasher.update([kind]);
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            let target = target.to_string_lossy();
            hasher.update((target.len() as u64).to_le_bytes());
            hasher.update(target.as_bytes());
        } else if metadata.is_file() {
            hasher.update(metadata.len().to_le_bytes());
            let mut file = std::fs::File::open(&path)?;
            let mut buffer = vec![0_u8; FILE_HASH_CHUNK_SIZE];
            loop {
                let bytes_read = file.read(&mut buffer)?;
                if bytes_read == 0 {
                    break;
                }
                hasher.update(&buffer[..bytes_read]);
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_directory_entries(directory: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        entries.push(path.clone());
        if metadata.is_dir() {
            collect_directory_entries(&path, entries)?;
        }
    }
    Ok(())
}

fn sha1_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha1::digest(bytes))
}

fn timestamp() -> String {
    Utc::now().to_rfc3339()
}

fn trim_to_last<T>(items: &mut Vec<T>, limit: usize) {
    if items.len() > limit {
        items.drain(..items.len() - limit);
    }
}

const fn completion_status_name(status: TaskCompletionStatus) -> &'static str {
    match status {
        TaskCompletionStatus::Passed => "passed",
        TaskCompletionStatus::Partial => "partial",
        TaskCompletionStatus::Blocked => "blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    async fn ledger_fixture() -> (TempDir, TaskEvidenceLedger) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(repo.join(".git"))
            .await
            .expect("git dir");
        tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
            .await
            .expect("manifest");
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
        let ledger =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        (temp, ledger)
    }

    fn text_source(text: &str) -> UserInput {
        UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }
    }

    async fn source_dossier(
        ledger: &TaskEvidenceLedger,
        candidate: Option<&str>,
    ) -> CompletionReviewDossier {
        ledger
            .completion_review_dossier(
                candidate,
                &[],
                &[],
                &ReviewLensSelectionFacts::default(),
                &[],
                true,
                true,
            )
            .await
            .expect("completion review dossier")
    }

    fn text_local_classification(requirement_spans: Vec<SourceSpan>) -> SourceLocalClassification {
        SourceLocalClassification {
            local_kind: SourceLocalClassificationKind::RequirementBearing,
            local_semantic_cues: requirement_spans
                .iter()
                .cloned()
                .map(|source_span| LocalSemanticCue {
                    kind: LocalSemanticCueKind::Assertion,
                    source_span: Some(source_span),
                })
                .collect(),
            requirement_spans,
            reason: "immutable source contains requirements".to_string(),
        }
    }

    fn resolved_requirement_source(
        source: &UserSourceRecord,
        requirement_spans: &[SourceSpan],
    ) -> ClassifiedSource {
        ClassifiedSource {
            source_id: source.source_id.clone(),
            kind: ClassifiedSourceKind::RequirementBearing,
            requirements: requirement_spans
                .iter()
                .cloned()
                .map(|source_span| ClassifiedRequirement {
                    source_span,
                    status: RequirementStatus::Active,
                    superseded_by: None,
                })
                .collect(),
            reason: None,
        }
    }

    fn plan(status: StepStatus) -> UpdatePlanArgs {
        UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg {
                id: Some("implement".to_string()),
                step: "Implement the runtime path".to_string(),
                status,
                depends_on: Vec::new(),
                acceptance_criteria: vec!["focused validation passes".to_string()],
                runtime_paths: vec!["src/lib.rs".to_string()],
                generated_artifacts: Vec::new(),
                risks: Vec::new(),
                requires_desktop_activation: false,
                validation_route: None,
            }],
        }
    }

    #[tokio::test]
    async fn legacy_completed_is_canonicalized_to_passed() {
        let (_temp, ledger) = ledger_fixture().await;
        let normalized = ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;
        assert_eq!(normalized.plan[0].status, StepStatus::Passed);
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Passed);
    }

    #[tokio::test]
    async fn missing_generation_and_desktop_activation_are_partial_conditions() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut update = plan(StepStatus::Completed);
        update.plan[0].generated_artifacts = vec!["generated/missing.json".to_string()];
        update.plan[0].requires_desktop_activation = true;
        ledger.record_plan_update(&update).await;
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Partial);
        assert!(
            gate.reasons
                .iter()
                .any(|reason| reason.contains("generated artifact"))
        );
        assert!(
            gate.reasons
                .iter()
                .any(|reason| reason.contains("Desktop activation"))
        );
        let routes = completion_review_locally_obtainable_proof_routes(&gate);
        assert!(routes.iter().any(|route| {
            route.contains("native host implements a publish-ID-bound initialization handshake")
        }));
        assert!(
            routes
                .iter()
                .all(|route| !route.contains("Run the supported publish/restart route"))
        );
    }

    #[tokio::test]
    async fn finalization_advisory_is_bounded_and_read_only() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::InProgress))
            .await;
        let revision = ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .revision;
        let warning = ledger.finalization_advisory().await.expect("warning");
        assert!(!warning.contains("No automatic repair turn was started"));
        assert!(ledger.finalization_advisory().await.is_some());
        assert_eq!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .revision,
            revision
        );
    }

    #[tokio::test]
    async fn compaction_task_state_retains_active_plan_without_proof_payloads() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::InProgress))
            .await;

        let state = ledger
            .compaction_task_state()
            .await
            .expect("compaction task state");

        assert!(state.contains("implement [in_progress active]"));
        assert!(state.contains("Implement the runtime path"));
        assert!(state.contains("## Goal"));
        assert!(state.contains("## Current state"));
        assert!(state.contains("## Completed work"));
        assert!(state.contains("## Unresolved work"));
        assert!(state.contains("## Evidence"));
        assert!(state.contains("## Next action"));
        assert!(approx_token_count(&state) <= COMPACTION_TASK_STATE_MAX_TOKENS);
    }

    #[tokio::test]
    async fn compaction_task_state_retains_completed_step_details() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;

        let state = ledger
            .compaction_task_state()
            .await
            .expect("compaction task state");

        assert!(state.contains("## Completed work"));
        assert!(state.contains("implement [passed]: Implement the runtime path"));
        assert!(state.contains("validation receipt:"));
    }

    #[tokio::test]
    async fn non_blocking_runtime_risk_is_a_warning_and_keeps_completion_partial() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;
        {
            let mut guard = ledger.document.lock().await;
            let document = guard.as_mut().expect("document");
            upsert_risk(
                document,
                EvidenceRisk {
                    id: "advisory-risk".to_string(),
                    description: "read-only command could not be classified".to_string(),
                    source: "command".to_string(),
                    blocking: false,
                    resolved: false,
                    epoch: document.evidence_epoch,
                },
            );
        }

        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Partial);
        assert_eq!(
            gate.reasons,
            vec!["read-only command could not be classified".to_string()]
        );

        let state = ledger
            .compaction_task_state()
            .await
            .expect("compaction task state");
        assert!(state.contains("## Warnings\n- risk advisory-risk"));
        let unresolved = state
            .split("## Unresolved work")
            .nth(1)
            .and_then(|tail| tail.split("## Warnings").next())
            .expect("unresolved work section");
        assert!(!unresolved.contains("advisory-risk"));
    }

    #[tokio::test]
    async fn locked_user_decisions_persist_supersession_and_omit_secrets() {
        use codex_protocol::request_user_input::RequestUserInputAnswer;
        use std::collections::HashMap;

        let (_temp, ledger) = ledger_fixture().await;
        let question = RequestUserInputQuestion {
            id: "deployment".to_string(),
            header: "Deployment".to_string(),
            question: "Where should this run?".to_string(),
            is_other: false,
            is_secret: false,
            options: None,
        };
        let secret = RequestUserInputQuestion {
            id: "token".to_string(),
            header: "Token".to_string(),
            question: "What is the token?".to_string(),
            is_other: false,
            is_secret: true,
            options: None,
        };
        let response = |deployment: &str| RequestUserInputResponse {
            answers: HashMap::from([
                (
                    "deployment".to_string(),
                    RequestUserInputAnswer {
                        answers: vec![deployment.to_string()],
                    },
                ),
                (
                    "token".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["do-not-store".to_string()],
                    },
                ),
            ]),
            interrupted: false,
        };

        ledger
            .record_locked_user_decisions(
                "call-1",
                "turn-1",
                &[question.clone(), secret.clone()],
                &response("staging"),
            )
            .await;
        ledger
            .record_locked_user_decisions(
                "call-2",
                "turn-2",
                &[question.clone(), secret.clone()],
                &response("production"),
            )
            .await;
        let unrelated_question = RequestUserInputQuestion {
            id: "deployment".to_string(),
            header: "Region".to_string(),
            question: "Which region should be used?".to_string(),
            is_other: false,
            is_secret: false,
            options: None,
        };
        ledger
            .record_locked_user_decisions(
                "call-3",
                "turn-3",
                &[unrelated_question, secret],
                &response("us-east"),
            )
            .await;

        {
            let guard = ledger.document.lock().await;
            let decisions = &guard.as_ref().expect("document").locked_user_decisions;
            assert_eq!(decisions.len(), 3);
            assert_eq!(
                decisions[1].supersedes.as_deref(),
                Some("call-1:deployment")
            );
            assert_eq!(decisions[2].supersedes, None);
        }

        let persisted: TaskEvidenceDocument = serde_json::from_slice(
            &tokio::fs::read(ledger.evidence_path.as_ref().expect("evidence path"))
                .await
                .expect("persisted evidence"),
        )
        .expect("persisted evidence document");
        assert_eq!(persisted.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(persisted.locked_user_decisions.len(), 3);
        assert!(
            persisted
                .locked_user_decisions
                .iter()
                .all(|decision| decision.question_id != "token")
        );

        let state = ledger
            .compaction_task_state()
            .await
            .expect("compaction task state");
        assert!(state.contains("## Locked decisions\n- Deployment: production"));
        assert!(state.contains("- Region: us-east"));
        assert!(!state.contains("staging"));
        assert!(!state.contains("do-not-store"));
    }

    fn repair_finding(id: &str, requirement_ids: &[&str]) -> CompletionReviewFindingReceipt {
        CompletionReviewFindingReceipt {
            finding_id: id.to_string(),
            requirement_ids: requirement_ids.iter().map(|id| (*id).to_string()).collect(),
            lens: "correctness".to_string(),
            contract_surface: "task-evidence".to_string(),
            severity: "blocking".to_string(),
            evidence: "focused evidence".to_string(),
            smallest_correction: "repair the bounded surface".to_string(),
            proof_route: "focused test".to_string(),
        }
    }

    fn repair_command(sequence: u64, identity: &str) -> RepairCommandDelta {
        RepairCommandDelta {
            sequence,
            receipt_id: format!("command-{sequence}"),
            command: format!("check-{sequence}"),
            cwd: "repo".to_string(),
            exit_code: Some(0),
            timed_out: false,
            implementation_identity: Some(identity.to_string()),
        }
    }

    fn repair_surface(identifier: &str) -> StructuredContractSurface {
        StructuredContractSurface {
            kind: "rust_symbol".to_string(),
            owner: "task_evidence".to_string(),
            identifier: identifier.to_string(),
        }
    }

    fn repair_baseline_fixture() -> RepairBaseline {
        RepairBaseline {
            path_states: vec![RepairPathState {
                path: "src/lib.rs".to_string(),
                exists: true,
                content_hash: Some("before".to_string()),
            }],
            command_sequence_high_water_mark: 2,
            command_bindings: vec![
                BaselineCommandBinding {
                    sequence: 1,
                    receipt_id: "command-1".to_string(),
                    implementation_identity: Some("before-identity".to_string()),
                },
                BaselineCommandBinding {
                    sequence: 2,
                    receipt_id: "command-2".to_string(),
                    implementation_identity: Some("before-identity".to_string()),
                },
            ],
            implementation_surfaces: vec![repair_surface("existing")],
            repair_scope: RepairScope {
                path_grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
                paths: vec![RepairPathScope::ExactFile {
                    path: "src/lib.rs".to_string(),
                }],
                surfaces: vec![repair_surface("existing"), repair_surface("permitted-new")],
                affected_requirement_ids: vec!["R1".to_string(), "R2".to_string()],
            },
            source_ledger_hash: "source".to_string(),
            requirement_manifest_hash: "manifest".to_string(),
            plan_structure_hash: "plan".to_string(),
            default_child_mutation_identities: Vec::new(),
            typed_mutation_identities: Vec::new(),
            external_evidence_ids: Vec::new(),
        }
    }

    #[test]
    fn repair_baseline_build_failure_omits_delta_metadata() {
        assert_eq!(
            bind_initial_repair_baseline_metadata(
                Err(RereviewFallbackReason::RequirementManifestChanged),
                Some("{}"),
            ),
            None
        );
    }

    #[test]
    fn repair_instruction_mismatch_omits_delta_metadata() {
        let baseline = repair_baseline_fixture();
        assert_eq!(
            bind_initial_repair_baseline_metadata(Ok(baseline.clone()), Some("{}")),
            None
        );

        let baseline_hash = repair_baseline_hash(&baseline);
        let instruction = serde_json::json!({
            "repair_baseline_hash": baseline_hash,
            "declared_repair_scope": &baseline.repair_scope,
        })
        .to_string();
        assert!(bind_initial_repair_baseline_metadata(Ok(baseline), Some(&instruction)).is_some());
    }

    #[test]
    fn affected_requirements_are_deterministic_and_fail_closed() {
        let active = ["R3", "R1", "R2"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let findings = vec![
            repair_finding("F1", &["R2", "R1", "R2"]),
            repair_finding("F2", &["R1"]),
        ];
        assert_eq!(
            derive_affected_requirement_ids(&active, &findings),
            Ok(vec!["R1".to_string(), "R2".to_string()])
        );
        assert_eq!(
            derive_affected_requirement_ids(&active, &[]),
            Ok(vec!["R1".to_string(), "R2".to_string(), "R3".to_string()])
        );
        assert_eq!(
            derive_affected_requirement_ids(&active, &[repair_finding("F3", &[])]),
            Err(RereviewFallbackReason::RequirementManifestChanged)
        );
        assert_eq!(
            derive_affected_requirement_ids(
                &active,
                &[repair_finding("F4", &["inactive-or-unknown"])]
            ),
            Err(RereviewFallbackReason::RequirementManifestChanged)
        );
    }

    #[test]
    fn repair_path_scope_uses_the_bounded_versioned_grammar() {
        assert_eq!(
            canonical_repair_path("src\\.\\lib.rs", false),
            Ok("src/lib.rs".to_string())
        );
        for invalid in ["../src/lib.rs", "C:\\src\\lib.rs", "/src/lib.rs"] {
            assert_eq!(
                canonical_repair_path(invalid, false),
                Err(RereviewFallbackReason::InvalidPath)
            );
        }

        let exact = RepairPathScope::ExactFile {
            path: "src/lib.rs".to_string(),
        };
        assert!(repair_scope_matches(&exact, "src/lib.rs").expect("exact scope"));
        assert!(!repair_scope_matches(&exact, "src/lib.rs.bak").expect("exact boundary"));
        let prefix = RepairPathScope::DirectoryPrefix {
            path: "src".to_string(),
        };
        assert!(repair_scope_matches(&prefix, "src/nested/lib.rs").expect("prefix scope"));
        assert!(!repair_scope_matches(&prefix, "src-other/lib.rs").expect("prefix boundary"));

        let single = RepairPathScope::GeneratedPattern {
            grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
            pattern: "generated/*/out.json".to_string(),
        };
        assert!(repair_scope_matches(&single, "generated/a/out.json").expect("wildcard"));
        assert!(
            !repair_scope_matches(&single, "generated/a/b/out.json").expect("bounded wildcard")
        );
        let recursive = RepairPathScope::GeneratedPattern {
            grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
            pattern: "generated/**/out.json".to_string(),
        };
        assert!(
            repair_scope_matches(&recursive, "generated/a/b/c/d/e/f/g/h/out.json")
                .expect("bounded recursive wildcard")
        );
        assert!(
            !repair_scope_matches(&recursive, "generated/a/b/c/d/e/f/g/h/i/out.json")
                .expect("recursive wildcard limit")
        );
        for (version, pattern) in [
            (REPAIR_PATH_GRAMMAR_VERSION + 1, "generated/*"),
            (REPAIR_PATH_GRAMMAR_VERSION, "generated/file?.json"),
            (REPAIR_PATH_GRAMMAR_VERSION, "generated/a*b.json"),
            (REPAIR_PATH_GRAMMAR_VERSION, "generated/**/**/out.json"),
        ] {
            assert_eq!(
                repair_scope_matches(
                    &RepairPathScope::GeneratedPattern {
                        grammar_version: version,
                        pattern: pattern.to_string(),
                    },
                    "generated/file1.json"
                ),
                Err(RereviewFallbackReason::UnsupportedPathGrammar)
            );
        }

        #[cfg(windows)]
        {
            assert_eq!(
                canonical_repair_path("SRC/Lib.rs", false),
                Ok("src/lib.rs".to_string())
            );
            assert_eq!(
                canonical_repair_path("src/é.rs", false),
                Err(RereviewFallbackReason::AmbiguousWindowsCase)
            );
        }
    }

    #[test]
    fn plan_status_is_non_structural_but_plan_contract_changes_are_structural() {
        let original = EvidencePlanStep {
            id: "implement".to_string(),
            revision: 1,
            step: "Implement the runtime path".to_string(),
            status: StepStatus::Implemented,
            depends_on: vec!["design".to_string()],
            acceptance_criteria: vec!["focused validation passes".to_string()],
            runtime_paths: vec!["src/lib.rs".to_string()],
            generated_artifacts: vec!["generated/schema.json".to_string()],
            risks: vec!["protocol".to_string()],
            requires_desktop_activation: false,
            validation_route: None,
            external_validation_route: None,
            validation_disposition: ValidationDisposition::NotRequired,
            source_owner: None,
            implementation_surfaces: Vec::new(),
            mutation_obligations: Vec::new(),
            validation_receipt_id: None,
            edit_paths: BTreeSet::from(["src/lib.rs".to_string()]),
        };
        let mut status_only = original.clone();
        status_only.status = StepStatus::Passed;
        assert_eq!(
            plan_structure_hash(std::slice::from_ref(&original)),
            plan_structure_hash(std::slice::from_ref(&status_only))
        );

        let mut changed_text = original.clone();
        changed_text.step = "Change the runtime contract".to_string();
        assert_ne!(
            plan_structure_hash(std::slice::from_ref(&original)),
            plan_structure_hash(std::slice::from_ref(&changed_text))
        );
        let mut changed_paths = original.clone();
        changed_paths.edit_paths.insert("src/other.rs".to_string());
        assert_ne!(
            plan_structure_hash(std::slice::from_ref(&original)),
            plan_structure_hash(std::slice::from_ref(&changed_paths))
        );
    }

    #[test]
    fn contained_delta_uses_sequence_bindings_and_permitted_surfaces_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo source directory");
        std::fs::write(repo.join("src/lib.rs"), "after").expect("candidate source");
        let baseline = repair_baseline_fixture();
        let baseline_hash = repair_baseline_hash(&baseline);
        let current = CurrentRepairSnapshot {
            repository_root: repo.to_string_lossy().into_owned(),
            path_states: vec![RepairPathState {
                path: "src/lib.rs".to_string(),
                exists: true,
                content_hash: Some("after".to_string()),
            }],
            command_receipts: vec![
                repair_command(3, "candidate"),
                repair_command(1, "before-identity"),
                repair_command(2, "before-identity"),
            ],
            plan_structure_hash: "plan".to_string(),
            declared_path_scopes: baseline.repair_scope.paths.clone(),
            implementation_surfaces: vec![
                repair_surface("permitted-new"),
                repair_surface("existing"),
            ],
            default_child_mutation_identities: vec![
                serde_json::json!({ "paths": ["src/lib.rs"] }).to_string(),
            ],
            typed_mutation_identities: vec![
                serde_json::json!({ "paths": ["src/lib.rs"] }).to_string(),
            ],
            external_evidence_ids: Vec::new(),
            containment_errors: Vec::new(),
        };
        let original_findings = vec![repair_finding("F1", &["R1"])];
        let input = build_rereview_input(
            Some(&baseline),
            Some(&baseline_hash),
            Some("repair-hash"),
            &original_findings,
            &current,
            "candidate",
            "source",
            "manifest",
        )
        .expect("rereview input");
        assert_eq!(input.input_mode, RereviewInputMode::Delta);
        assert!(input.fallback_reasons.is_empty());
        let delta = input.delta.expect("contained delta");
        assert_eq!(
            delta
                .new_command_receipts
                .iter()
                .map(|receipt| receipt.sequence)
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(
            delta
                .invalidated_command_receipts
                .iter()
                .map(|receipt| receipt.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(delta.path_changes.len(), 1);
        assert_eq!(
            delta.newly_realized_surfaces,
            vec![repair_surface("permitted-new")]
        );
        assert_eq!(input.delta_hash, Some(repair_delta_hash(&delta)));
        assert!(validate_repair_delta_contents(&delta, &baseline, &original_findings).is_ok());
        let mut tampered = delta;
        tampered.affected_requirement_ids.push("R2".to_string());
        assert!(validate_repair_delta_contents(&tampered, &baseline, &original_findings).is_err());
    }

    #[test]
    fn full_fallback_collects_all_reasons_in_stable_enum_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo source directory");
        let baseline = repair_baseline_fixture();
        let current = CurrentRepairSnapshot {
            repository_root: repo.to_string_lossy().into_owned(),
            path_states: vec![RepairPathState {
                path: "outside.rs".to_string(),
                exists: true,
                content_hash: Some("outside".to_string()),
            }],
            command_receipts: vec![
                repair_command(1, "before-identity"),
                repair_command(2, "before-identity"),
            ],
            plan_structure_hash: "changed-plan".to_string(),
            declared_path_scopes: baseline.repair_scope.paths.clone(),
            implementation_surfaces: vec![repair_surface("outside")],
            default_child_mutation_identities: Vec::new(),
            typed_mutation_identities: Vec::new(),
            external_evidence_ids: vec!["new-evidence".to_string()],
            containment_errors: vec![RereviewFallbackReason::UnrepresentableEvidenceChange],
        };
        let input = build_rereview_input(
            Some(&baseline),
            Some("wrong-baseline-hash"),
            Some("repair-hash"),
            &[repair_finding("F1", &["R1"])],
            &current,
            "candidate",
            "changed-source",
            "changed-manifest",
        )
        .expect("fallback input");
        assert_eq!(input.input_mode, RereviewInputMode::FullFallback);
        assert!(input.delta.is_none());
        assert!(input.delta_hash.is_none());
        assert_eq!(
            input.fallback_reasons,
            vec![
                RereviewFallbackReason::InvalidBaselineHash,
                RereviewFallbackReason::PathOutsideScope,
                RereviewFallbackReason::SourceIdentityChanged,
                RereviewFallbackReason::RequirementManifestChanged,
                RereviewFallbackReason::PlanStructureChanged,
                RereviewFallbackReason::ContractSurfaceOutsideScope,
                RereviewFallbackReason::UnrepresentableEvidenceChange,
            ]
        );
    }

    #[test]
    fn source_classification_cache_tolerates_bad_elements_and_invalidates_duplicate_keys() {
        #[derive(Deserialize)]
        struct CacheEnvelope {
            #[serde(deserialize_with = "deserialize_source_classification_cache")]
            source_classification_cache: Vec<SourceClassificationCacheEntry>,
        }

        let entry =
            |content_hash: String, start: usize, end: usize| SourceClassificationCacheEntry {
                contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
                source_kind: UserSourceKind::Text,
                content_hash,
                classification: text_local_classification(vec![SourceSpan::Text { start, end }]),
            };
        let duplicate = entry("1".repeat(64), 0, 1);
        let malformed_duplicate = entry("e".repeat(64), 0, 1);
        let later = entry("f".repeat(64), 1, 2);
        let earlier = entry("0".repeat(64), 0, 1);
        let decoded: CacheEnvelope = serde_json::from_value(serde_json::json!({
            "source_classification_cache": [
                later,
                {"not": "a cache entry"},
                duplicate.clone(),
                earlier,
                duplicate,
                malformed_duplicate,
                {
                    "contract_version": SOURCE_CLASSIFICATION_CONTRACT_VERSION,
                    "source_kind": "text",
                    "content_hash": "e".repeat(64),
                    "classification": {"local_kind": "not_a_kind"}
                },
            ]
        }))
        .expect("tolerant cache envelope");

        assert_eq!(decoded.source_classification_cache.len(), 2);
        assert_eq!(
            decoded
                .source_classification_cache
                .iter()
                .map(|entry| entry.content_hash.clone())
                .collect::<Vec<_>>(),
            vec!["0".repeat(64), "f".repeat(64)]
        );
    }

    #[test]
    fn persisted_local_projection_contains_no_dossier_relative_fields() {
        let entry = SourceClassificationCacheEntry {
            contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
            source_kind: UserSourceKind::Text,
            content_hash: "a".repeat(64),
            classification: text_local_classification(vec![SourceSpan::Text { start: 0, end: 9 }]),
        };
        let encoded = serde_json::to_string(&entry).expect("serialize cache entry");

        for forbidden in [
            "requirement_status",
            "superseded_by",
            "source_id",
            "requirement_id",
            "source_ordinal",
            "completion_epoch",
            "manifest_revision",
        ] {
            assert!(!encoded.contains(forbidden), "unexpected field {forbidden}");
        }
    }

    #[tokio::test]
    async fn duplicate_source_occurrences_share_one_cache_entry_but_materialize_separately() {
        let (_temp, ledger) = ledger_fixture().await;
        let source = text_source("Use YAML.");
        assert!(
            ledger
                .record_user_sources("message-1", std::slice::from_ref(&source))
                .await
        );
        assert!(ledger.record_user_sources("message-2", &[source]).await);
        let dossier = source_dossier(&ledger, None).await;
        assert_eq!(dossier.sources.len(), 2);
        assert_eq!(
            source_classification_cache_key(&dossier.sources[0]),
            source_classification_cache_key(&dossier.sources[1])
        );
        let span = SourceSpan::Text { start: 0, end: 9 };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let materialization = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key,
                text_local_classification(vec![span.clone()]),
            )]),
            resolved_sources: dossier
                .sources
                .iter()
                .map(|source| resolved_requirement_source(source, std::slice::from_ref(&span)))
                .collect(),
        };

        assert_eq!(
            ledger
                .apply_source_classification(&dossier, materialization)
                .await,
            AtomicReviewTransition::Persisted(())
        );
        let refreshed = source_dossier(&ledger, None).await;
        assert_eq!(refreshed.source_classification_cache.len(), 1);
        assert_eq!(refreshed.requirements.len(), 2);
        assert_ne!(
            refreshed.requirements[0].requirement_id,
            refreshed.requirements[1].requirement_id
        );
        assert!(refreshed.mappings_classified);
    }

    #[tokio::test]
    async fn resolver_version_transition_can_change_status_without_rewriting_identity_or_history() {
        let (_temp, ledger) = ledger_fixture().await;
        assert!(
            ledger
                .record_user_sources("message-1", &[text_source("Use YAML.")])
                .await
        );
        let dossier = source_dossier(&ledger, None).await;
        let span = SourceSpan::Text { start: 0, end: 9 };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let initial = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key.clone(),
                text_local_classification(vec![span.clone()]),
            )]),
            resolved_sources: vec![resolved_requirement_source(
                &dossier.sources[0],
                std::slice::from_ref(&span),
            )],
        };
        assert_eq!(
            ledger.apply_source_classification(&dossier, initial).await,
            AtomicReviewTransition::Persisted(())
        );

        let mut transition_dossier = source_dossier(&ledger, None).await;
        let original_requirement = transition_dossier.requirements[0].clone();
        let original_manifest_revision = transition_dossier.manifest_revision;
        transition_dossier.relationship_resolution_current = false;
        transition_dossier.mappings_classified = false;
        let replacement = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key,
                text_local_classification(vec![span.clone()]),
            )]),
            resolved_sources: vec![ClassifiedSource {
                source_id: transition_dossier.sources[0].source_id.clone(),
                kind: ClassifiedSourceKind::RequirementBearing,
                requirements: vec![ClassifiedRequirement {
                    source_span: span,
                    status: RequirementStatus::Withdrawn,
                    superseded_by: None,
                }],
                reason: None,
            }],
        };
        assert_eq!(
            ledger
                .apply_source_classification(&transition_dossier, replacement)
                .await,
            AtomicReviewTransition::Persisted(())
        );

        let refreshed = source_dossier(&ledger, None).await;
        assert_eq!(refreshed.manifest_revision, original_manifest_revision + 1);
        assert_eq!(refreshed.requirements.len(), 1);
        let rematerialized = &refreshed.requirements[0];
        assert_eq!(
            rematerialized.requirement_id,
            original_requirement.requirement_id
        );
        assert_eq!(rematerialized.source_id, original_requirement.source_id);
        assert_eq!(
            rematerialized.source_content_hash,
            original_requirement.source_content_hash
        );
        assert_eq!(rematerialized.source_span, original_requirement.source_span);
        assert_eq!(
            rematerialized.exact_material,
            original_requirement.exact_material
        );
        assert_eq!(rematerialized.status, RequirementStatus::Withdrawn);
        let guard = ledger.document.lock().await;
        let history = &guard
            .as_ref()
            .expect("document")
            .completion_review_v2
            .as_ref()
            .expect("completion review ledger")
            .manifest_snapshots;
        assert!(history.iter().any(|snapshot| {
            snapshot.manifest_revision == original_manifest_revision
                && snapshot.requirements == vec![original_requirement.clone()]
        }));
    }

    #[tokio::test]
    async fn non_authoring_materialization_failure_is_atomic() {
        let (_temp, ledger) = ledger_fixture().await;
        assert!(
            ledger
                .record_user_sources("message-1", &[text_source("Use YAML.")])
                .await
        );
        let dossier = source_dossier(&ledger, None).await;
        let span = SourceSpan::Text { start: 0, end: 9 };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let mut resolved = resolved_requirement_source(&dossier.sources[0], &[]);
        resolved.kind = ClassifiedSourceKind::RequirementBearing;
        let invalid = SourceMaterialization {
            local_classifications: BTreeMap::from([(key, text_local_classification(vec![span]))]),
            resolved_sources: vec![resolved],
        };

        assert_eq!(
            ledger.apply_source_classification(&dossier, invalid).await,
            AtomicReviewTransition::Failed
        );
        let refreshed = source_dossier(&ledger, None).await;
        assert_eq!(refreshed.document_revision, dossier.document_revision);
        assert!(refreshed.source_classification_cache.is_empty());
        assert!(refreshed.requirements.is_empty());
        assert!(!refreshed.mappings_classified);
    }

    #[tokio::test]
    async fn manifest_gap_correction_updates_cache_for_later_reconstruction() {
        let (_temp, ledger) = ledger_fixture().await;
        let material = "Use JSON. Encrypt it.";
        assert!(
            ledger
                .record_user_sources("message-1", &[text_source(material)])
                .await
        );
        let dossier = source_dossier(&ledger, None).await;
        let first = SourceSpan::Text { start: 0, end: 9 };
        let omitted = SourceSpan::Text {
            start: 10,
            end: material.len(),
        };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let initial = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key.clone(),
                text_local_classification(vec![first.clone()]),
            )]),
            resolved_sources: vec![resolved_requirement_source(
                &dossier.sources[0],
                std::slice::from_ref(&first),
            )],
        };
        assert_eq!(
            ledger.apply_source_classification(&dossier, initial).await,
            AtomicReviewTransition::Persisted(())
        );

        let classified = source_dossier(&ledger, None).await;
        let gaps = vec![ManifestGapInput {
            source_id: classified.sources[0].source_id.clone(),
            omitted_spans: vec![omitted.clone()],
        }];
        let corrected = source_local_classifications_with_manifest_gaps(&classified, &gaps)
            .expect("corrected local facts");
        assert_eq!(
            corrected
                .get(&key)
                .expect("corrected key")
                .requirement_spans,
            vec![first.clone(), omitted.clone()]
        );
        let replacement = SourceMaterialization {
            local_classifications: corrected,
            resolved_sources: vec![resolved_requirement_source(
                &classified.sources[0],
                &[first, omitted.clone()],
            )],
        };
        assert_eq!(
            ledger
                .apply_source_classification(&classified, replacement)
                .await,
            AtomicReviewTransition::Persisted(())
        );

        let reconstructed = source_dossier(&ledger, None).await;
        assert_eq!(reconstructed.requirements.len(), 2);
        assert!(
            reconstructed
                .source_classification_cache
                .get(&key)
                .expect("persisted corrected cache entry")
                .requirement_spans
                .contains(&omitted)
        );
    }

    #[test]
    fn symlink_escape_is_uncontained() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&repo).expect("repo directory");
        std::fs::create_dir_all(&outside).expect("outside directory");
        std::fs::write(outside.join("file.rs"), "outside").expect("outside file");
        let link = repo.join("link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("directory symlink");
        #[cfg(windows)]
        if let Err(err) = std::os::windows::fs::symlink_dir(&outside, &link) {
            if err.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("directory symlink: {err}");
        }

        assert_eq!(
            path_resolves_within_repository(&repo.to_string_lossy(), "link/file.rs"),
            Err(RereviewFallbackReason::SymlinkEscape)
        );
    }
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
