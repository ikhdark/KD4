use chrono::Utc;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::ThreadId;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::user_input::UserInput;
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
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::warn;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 5;
const FROZEN_TASK_EVIDENCE_V4_SCHEMA_VERSION: u32 = 4;
const TASK_EVIDENCE_COMPLETION_MODEL_VERSION: u32 = 3;
const FILE_HASH_CHUNK_SIZE: usize = 64 * 1024;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_EXTERNAL_EVIDENCE_RECEIPTS: usize = 256;
#[cfg(test)]
const MAX_COMPLETION_REVIEW_RECEIPTS: usize = 256;
const MAX_ATTRIBUTED_WORKSPACE_EVENTS: usize = 256;
const EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES: usize = 16 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES: usize = 8 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_HEADER: &str =
    "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1\n";
const USER_SOURCE_LEDGER_CANONICAL_FORMAT: &str = "KD4_USER_SOURCE_LEDGER_CANONICAL_V1";
const REQUIREMENT_MANIFEST_CANONICAL_FORMAT: &str = "KD4_REQUIREMENT_MANIFEST_CANONICAL_V1";
const IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT: &str = "KD4_IMPLEMENTATION_IDENTITY_CANONICAL_V1";
const DOSSIER_SNAPSHOT_CANONICAL_FORMAT: &str = "KD4_DOSSIER_SNAPSHOT_CANONICAL_V1";
const REPAIR_INSTRUCTION_CANONICAL_FORMAT: &str = "KD4_REPAIR_INSTRUCTION_CANONICAL_V1";
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
    last_persisted_revision: Arc<AtomicU64>,
    source_capture_failed: Arc<AtomicBool>,
    #[cfg(test)]
    persistence_test_control: Arc<std::sync::Mutex<Option<PersistenceTestControl>>>,
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
    plan: Vec<EvidencePlanStep>,
    active_step_id: Option<String>,
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
    completion: Option<TaskCompletionGate>,
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
    source_capture_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskAttributedWorkspaceEvent {
    workspace_id: String,
    epoch: u64,
    actor_id: String,
    paths: Vec<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    mapping: SourceMapping,
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
    findings: Vec<CompletionReviewFindingReceipt>,
    dispositions: Vec<CompletionReviewDispositionReceipt>,
    #[serde(default)]
    manifest_gaps: Vec<ManifestGapInput>,
    repair_instruction_hash: Option<String>,
    infrastructure_outcome: String,
    review_clean: bool,
    terminal_outcome: Option<String>,
    recorded_at: String,
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
    step: String,
    status: StepStatus,
    depends_on: Vec<String>,
    acceptance_criteria: Vec<String>,
    runtime_paths: Vec<String>,
    generated_artifacts: Vec<String>,
    risks: Vec<String>,
    requires_desktop_activation: bool,
    edit_paths: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EditIntent {
    call_id: String,
    step_id: Option<String>,
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
    command: Vec<String>,
    cwd: String,
    exit_code: i32,
    timed_out: bool,
    duration_ms: u64,
    possible_mutation: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DesktopActivationReceipt {
    epoch: u64,
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
    desktop_process_id: u32,
    #[serde(default)]
    desktop_process_identity: String,
    #[serde(default)]
    desktop_executable_path: String,
    #[serde(default)]
    post_restart_initialization_observation: String,
    #[serde(default)]
    observation_timestamp: String,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
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
        source_capture_failed: false,
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
                plan: Vec::new(),
                active_step_id: None,
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
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            source_capture_failed: Arc::new(AtomicBool::new(source_capture_failed)),
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
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            source_capture_failed: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            persistence_test_control: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> TaskEvidenceMode {
        self.mode
    }

    pub(crate) fn allows_kd4_completion(&self) -> bool {
        self.mode == TaskEvidenceMode::Kd4Completion
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
                    if ledger.workspace_event_baseline_epoch == ledger.completion_epoch {
                        return true;
                    }
                    let completion_epoch = ledger.completion_epoch;
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        return false;
                    };
                    ledger.last_workspace_event_epoch = workspace_epoch;
                    ledger.workspace_event_baseline_epoch = completion_epoch;
                    ledger.typed_assignment_baseline = typed_assignment_baseline;
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

    pub(crate) async fn record_plan_update(&self, update: &UpdatePlanArgs) -> UpdatePlanArgs {
        if !self.allows_kd4_completion() {
            return update.clone();
        }
        let Some((response, snapshot)) = self
            .update_document(|document| {
                let previous = document
                    .plan
                    .iter()
                    .cloned()
                    .map(|step| (step.id.clone(), step))
                    .collect::<BTreeMap<_, _>>();
                let mut used_ids = BTreeSet::new();
                let mut normalized = Vec::with_capacity(update.plan.len());
                let mut material_plan_change = previous.len() != update.plan.len();
                let mut duplicate_explicit_ids = BTreeSet::new();
                let mut seen_explicit_ids = BTreeSet::new();
                for (index, item) in update.plan.iter().enumerate() {
                    if let Some(id) = item.id.as_ref()
                        && !seen_explicit_ids.insert(id.clone())
                    {
                        duplicate_explicit_ids.insert(id.clone());
                    }
                    let id = effective_step_id(item, index, &mut used_ids);
                    let old = previous.get(&id);
                    let material_step_change =
                        old.is_none_or(|step| !step_materially_matches_item(step, item));
                    material_plan_change |= material_step_change;
                    let status = normalize_requested_status(&item.status);
                    normalized.push(EvidencePlanStep {
                        id,
                        step: item.step.clone(),
                        status,
                        depends_on: item.depends_on.clone(),
                        acceptance_criteria: item.acceptance_criteria.clone(),
                        runtime_paths: item.runtime_paths.clone(),
                        generated_artifacts: item.generated_artifacts.clone(),
                        risks: item.risks.clone(),
                        requires_desktop_activation: item.requires_desktop_activation,
                        edit_paths: old
                            .filter(|_| !material_step_change)
                            .map_or_else(BTreeSet::new, |step| step.edit_paths.clone()),
                    });
                }
                material_plan_change |= previous
                    .keys()
                    .any(|id| !normalized.iter().any(|step| &step.id == id));
                document.plan = normalized;
                if material_plan_change {
                    invalidate_for_plan_change(document);
                }
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                rebuild_declared_requirements_and_risks(document);
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                if plan_is_terminally_acknowledged(document) {
                    resolve_recoverable_runtime_risks(document);
                }
                document.updated_at = timestamp();
                document.completion = None;
                UpdatePlanArgs {
                    explanation: update.explanation.clone(),
                    plan: document.plan.iter().map(plan_item_from_evidence).collect(),
                }
            })
            .await
        else {
            return update.clone();
        };
        self.persist_document(&snapshot).await;
        response
    }

    #[cfg(test)]
    pub(crate) async fn record_edit_intent(&self, call_id: &str, cwd: &Path, paths: &[PathBuf]) {
        self.record_edit_intent_with_provenance(call_id, cwd, paths, None)
            .await;
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
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let normalized = normalize_input_path(repo_root, Some(cwd), path);
            files.push(snapshot_file(repo_root, &normalized).await);
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files.dedup_by(|left, right| left.path == right.path);
        let evidence_call_id = provenance
            .map(|value| format!("{}:{call_id}", value.source_thread_id))
            .unwrap_or_else(|| call_id.to_string());

        let Some((_, snapshot)) = self
            .update_document(|document| {
                document
                    .edit_intents
                    .retain(|intent| intent.call_id != evidence_call_id);
                document.edit_intents.push(EditIntent {
                    call_id: evidence_call_id,
                    step_id: document.active_step_id.clone(),
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
                    invalidate_for_mutation(document);
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
                            if edit_succeeded
                                && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                            {
                                step.status = StepStatus::Implemented;
                            }
                        }
                    }
                    if affected_steps.is_empty() {
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
            provenance,
            None,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command_bound_with_provenance(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
    ) {
        if self.mode == TaskEvidenceMode::EvidenceOnly {
            if possible_mutation {
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
                if possible_mutation {
                    invalidate_for_mutation(document);
                    let epoch = document.evidence_epoch;
                    if let Some(active_step_id) = document.active_step_id.clone()
                        && let Some(step) = document
                            .plan
                            .iter_mut()
                            .find(|step| step.id == active_step_id)
                        && command_succeeded
                        && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                    {
                        step.status = StepStatus::Implemented;
                    }
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("unknown-command-mutation-{epoch}"),
                            description:
                                "a command may have mutated files without exact path/hash attribution"
                                    .to_string(),
                            source: "command".to_string(),
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
                document.command_receipts.push(CommandReceipt {
                    id: receipt_id,
                    recorded_at: timestamp(),
                    epoch: document.evidence_epoch,
                    step_id: document.active_step_id.clone(),
                    command: command.to_vec(),
                    cwd: cwd.to_string(),
                    exit_code,
                    timed_out,
                    duration_ms,
                    possible_mutation,
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
                    source_thread_id: provenance.map(|value| value.source_thread_id.clone()),
                    source_agent_path: provenance.map(|value| value.source_agent_path.clone()),
                });
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

    pub(crate) async fn finalization_advisory(&self) -> Option<String> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let gate = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            task_is_tracked(document)
                .then(|| derive_completion_gate(document, self.evidence_path.as_deref()))?
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
        Some(format!(
            "KD4 task evidence is {status}: {reason_summary}{remaining}.",
            status = completion_status_name(gate.status),
        ))
    }

    pub(crate) async fn completion_review_dossier(
        &self,
        candidate_completion: Option<&str>,
        typed_mutation_identities: &[String],
        typed_evidence: &[String],
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
        let (
            document_revision,
            root_task_id,
            completion_epoch,
            manifest_revision,
            sources,
            source_mappings,
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
            let source_mappings = active_source_mappings(ledger);
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
                        && receipt.implementation_identity_hash.as_deref()
                            == Some(implementation_identity_hash.as_str())
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
            let evidence_gate = derive_completion_gate(document, self.evidence_path.as_deref());
            let locally_obtainable_proof_routes =
                completion_review_locally_obtainable_proof_routes(&evidence_gate);
            let desktop_activation =
                document
                    .desktop_activation_receipt
                    .as_ref()
                    .filter(|receipt| {
                        desktop_activation_receipt_is_complete(receipt)
                            && receipt.implementation_identity_hash.as_deref()
                                == Some(implementation_identity_hash.as_str())
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
            )
        };
        let mappings_classified = sources.iter().all(|source| {
            source_mappings
                .get(&source.source_id)
                .is_some_and(|mapping| !matches!(mapping, SourceMapping::PendingClassification))
        });
        Some(CompletionReviewDossier {
            document_revision,
            root_task_id,
            completion_epoch,
            manifest_revision,
            sources,
            source_mappings,
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
        if input.attempt_kind == CompletionReviewAttemptKind::TerminalClosure {
            return AtomicReviewTransition::Failed;
        }
        let reconstruct_manifest = !input.manifest_gaps.is_empty();
        let gap_additions = if reconstruct_manifest {
            let Some(additions) = manifest_gap_additions(dossier, &input.manifest_gaps) else {
                return AtomicReviewTransition::Failed;
            };
            Some(additions)
        } else {
            None
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
                    || input.repair_instruction_hash.is_some()
                    || input.infrastructure_outcome != "ok"
                    || input.terminal_outcome.is_some()))
            || input.infrastructure_outcome.trim().is_empty()
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
            finding.requirement_ids.is_empty()
                || unique_requirement_ids.len() != finding.requirement_ids.len()
                || !unique_requirement_ids
                    .iter()
                    .any(|requirement_id| active_requirement_ids.contains(requirement_id))
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
        let needs_correction =
            input.repair_instruction.is_some() || input.repair_instruction_hash.is_some();
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
                        && !needs_correction
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
        let attempt_kind = input.attempt_kind;
        let parent_review_id = input.parent_review_id.clone();
        let superseded_review_id = input.superseded_review_id.clone();
        let dispositions = input.dispositions.clone();
        let manifest_gaps = input.manifest_gaps.clone();
        let infrastructure_outcome = input.infrastructure_outcome.clone();
        let terminal_outcome = input.terminal_outcome.clone();
        let review_clean = input.review_clean;
        let evidence_path = self
            .evidence_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        self.atomic_review_update(dossier.document_revision, None, None, move |document| {
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
                findings: findings.clone(),
                dispositions,
                manifest_gaps: manifest_gaps.clone(),
                repair_instruction_hash,
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
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction_hash: None,
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: Some(outcome.to_string()),
                    recorded_at: timestamp(),
                });
                terminal_review_id
            });
            if reconstruct_manifest {
                let previous_mappings = active_source_mappings(ledger);
                let mut requirements = active_manifest(ledger)
                    .map(|manifest| manifest.requirements.clone())
                    .unwrap_or_default();
                requirements.extend(gap_additions.unwrap_or_default());
                requirements.sort_by(|left, right| left.requirement_id.cmp(&right.requirement_id));
                let requirement_ids_by_source = requirements.iter().fold(
                    BTreeMap::<String, Vec<String>>::new(),
                    |mut by_source, requirement| {
                        by_source
                            .entry(requirement.source_id.clone())
                            .or_default()
                            .push(requirement.requirement_id.clone());
                        by_source
                    },
                );
                let new_revision = ledger.manifest_revision.saturating_add(1);
                for source in ledger
                    .source_records
                    .values()
                    .filter(|source| source.completion_epoch == ledger.completion_epoch)
                {
                    let mapping = match requirement_ids_by_source.get(&source.source_id) {
                        Some(requirement_ids) => {
                            let mut requirement_ids = requirement_ids.clone();
                            requirement_ids.sort();
                            SourceMapping::RequirementBearing { requirement_ids }
                        }
                        None => previous_mappings
                            .get(&source.source_id)
                            .cloned()
                            .unwrap_or(SourceMapping::PendingClassification),
                    };
                    ledger.mapping_revisions.push(SourceMappingRevision {
                        completion_epoch: ledger.completion_epoch,
                        manifest_revision: new_revision,
                        source_id: source.source_id.clone(),
                        mapping,
                    });
                }
                let manifest_hash = requirement_manifest_hash(new_revision, &requirements);
                let correction_consumed = ledger
                    .active_review_cycle
                    .as_ref()
                    .is_some_and(|cycle| cycle.correction_consumed);
                let parent_terminal_review_id = ledger
                    .active_review_cycle
                    .as_ref()
                    .and_then(|cycle| cycle.parent_terminal_review_id.clone());
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
                document.evidence_epoch = document.evidence_epoch.saturating_add(1);
                document.desktop_activation_receipt = None;
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
                }
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
            || !dossier.typed_quiescent
            || !dossier.default_children_quiescent
            || dossier.evidence_gate.status != TaskCompletionStatus::Passed
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
            let Some(accepted) = ledger
                .receipts
                .iter()
                .find(|receipt| receipt.review_id == accepted_review_id)
            else {
                return AtomicReviewTransition::Failed;
            };
            let activation_required = document
                .plan
                .iter()
                .any(|step| step.requires_desktop_activation);
            let activation_current = !activation_required
                || document
                    .desktop_activation_receipt
                    .as_ref()
                    .is_some_and(|receipt| {
                        desktop_activation_receipt_is_complete(receipt)
                            && receipt.epoch == document.evidence_epoch
                            && receipt.implementation_identity_hash.as_deref()
                                == Some(dossier.implementation_identity_hash.as_str())
                    });
            if document.revision != dossier.document_revision
                || !ledger.review_risk.unresolved
                || cycle.phase != CompletionReviewCyclePhase::ProvisionalClean
                || cycle.accepted_review_id.as_deref() != Some(accepted_review_id.as_str())
                || cycle.accepted_dossier_snapshot_id.as_deref()
                    != Some(dossier.dossier_snapshot_id.as_str())
                || !accepted.review_clean
                || accepted.terminal_outcome.is_some()
                || accepted.implementation_identity_hash != dossier.implementation_identity_hash
                || accepted.dossier_snapshot_id != dossier.dossier_snapshot_id
                || !activation_current
            {
                return AtomicReviewTransition::Superseded;
            }
        }

        let accepted_parent = accepted_review_id.clone();
        let completion = TaskCompletionGate {
            status: TaskCompletionStatus::Passed,
            reasons: Vec::new(),
            evidence_path: self
                .evidence_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        let transition = self
            .atomic_review_update(
                dossier.document_revision,
                Some(&dossier.implementation_identity_hash),
                Some(&dossier.dossier_snapshot_id),
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
                        dossier_snapshot_id: dossier.dossier_snapshot_id.clone(),
                        user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                        requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                        findings: Vec::new(),
                        dispositions: Vec::new(),
                        manifest_gaps: Vec::new(),
                        repair_instruction_hash: None,
                        infrastructure_outcome: "ok".to_string(),
                        review_clean: true,
                        terminal_outcome: Some("passed".to_string()),
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
        document.revision == dossier.document_revision
            && document
                .completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
            && cycle.phase == CompletionReviewCyclePhase::Closed
            && !ledger.review_risk.unresolved
            && cycle.accepted_dossier_snapshot_id.as_deref()
                == Some(dossier.dossier_snapshot_id.as_str())
            && terminal.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
            && terminal.terminal_outcome.as_deref() == Some("passed")
            && terminal.parent_review_id.as_deref() == Some(accepted_review_id)
            && terminal.implementation_identity_hash == dossier.implementation_identity_hash
            && terminal.dossier_snapshot_id == dossier.dossier_snapshot_id
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
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction_hash: None,
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
        classifications: Vec<ClassifiedSource>,
    ) -> AtomicReviewTransition<()> {
        let expected_ids = dossier
            .sources
            .iter()
            .map(|source| source.source_id.clone())
            .collect::<BTreeSet<_>>();
        let returned_ids = classifications
            .iter()
            .map(|classification| classification.source_id.clone())
            .collect::<BTreeSet<_>>();
        if expected_ids.len() != classifications.len() || expected_ids != returned_ids {
            return AtomicReviewTransition::Failed;
        }
        let sources = dossier
            .sources
            .iter()
            .map(|source| (source.source_id.clone(), source.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut requirement_ids = BTreeMap::<ClassifiedRequirementRef, String>::new();
        for classification in &classifications {
            let Some(source) = sources.get(&classification.source_id) else {
                return AtomicReviewTransition::Failed;
            };
            let valid_shape = match classification.kind {
                ClassifiedSourceKind::RequirementBearing => !classification.requirements.is_empty(),
                ClassifiedSourceKind::NonRequirement | ClassifiedSourceKind::SupersededContext => {
                    classification.requirements.is_empty()
                        && classification
                            .reason
                            .as_deref()
                            .is_some_and(|reason| !reason.trim().is_empty())
                }
                ClassifiedSourceKind::UnavailableOrTruncated => {
                    classification.requirements.is_empty()
                }
            };
            if !valid_shape {
                return AtomicReviewTransition::Failed;
            }
            for requirement in &classification.requirements {
                if material_for_span(source, &requirement.source_span).is_none() {
                    return AtomicReviewTransition::Failed;
                }
                let reference = ClassifiedRequirementRef {
                    source_id: source.source_id.clone(),
                    source_span: requirement.source_span.clone(),
                };
                let requirement_id = deterministic_requirement_id(source, &requirement.source_span);
                if requirement_ids.insert(reference, requirement_id).is_some() {
                    return AtomicReviewTransition::Failed;
                }
            }
        }
        if classifications.iter().any(|classification| {
            classification
                .requirements
                .iter()
                .any(|requirement| match requirement.status {
                    RequirementStatus::Active | RequirementStatus::Withdrawn => {
                        requirement.superseded_by.is_some()
                    }
                    RequirementStatus::Superseded => requirement
                        .superseded_by
                        .as_ref()
                        .is_none_or(|reference| !requirement_ids.contains_key(reference)),
                })
        }) {
            return AtomicReviewTransition::Failed;
        }

        let mut requirements = Vec::new();
        let mut mappings = Vec::new();
        for classification in classifications {
            let Some(source) = sources.get(&classification.source_id) else {
                return AtomicReviewTransition::Failed;
            };
            let mut mapped_requirement_ids = Vec::new();
            for requirement in classification.requirements {
                let reference = ClassifiedRequirementRef {
                    source_id: source.source_id.clone(),
                    source_span: requirement.source_span.clone(),
                };
                let Some(requirement_id) = requirement_ids.get(&reference).cloned() else {
                    return AtomicReviewTransition::Failed;
                };
                let Some(exact_material) = material_for_span(source, &requirement.source_span)
                else {
                    return AtomicReviewTransition::Failed;
                };
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
                        .and_then(|reference| requirement_ids.get(reference))
                        .cloned(),
                });
            }
            mapped_requirement_ids.sort();
            let mapping = match classification.kind {
                ClassifiedSourceKind::RequirementBearing => SourceMapping::RequirementBearing {
                    requirement_ids: mapped_requirement_ids,
                },
                ClassifiedSourceKind::NonRequirement => SourceMapping::NonRequirement {
                    reason: classification.reason.unwrap_or_default(),
                },
                ClassifiedSourceKind::SupersededContext => SourceMapping::SupersededContext {
                    reason: classification.reason.unwrap_or_default(),
                },
                ClassifiedSourceKind::UnavailableOrTruncated => {
                    SourceMapping::UnavailableOrTruncated
                }
            };
            mappings.push((source.source_id.clone(), mapping));
        }
        requirements.sort_by(|left, right| left.requirement_id.cmp(&right.requirement_id));
        if !requirement_supersession_is_acyclic(&requirements) {
            return AtomicReviewTransition::Failed;
        }
        let next_requirements = requirements
            .iter()
            .map(|requirement| (requirement.requirement_id.as_str(), requirement))
            .collect::<BTreeMap<_, _>>();
        for previous in &dossier.requirements {
            let Some(next) = next_requirements.get(previous.requirement_id.as_str()) else {
                return AtomicReviewTransition::Failed;
            };
            if next.source_id != previous.source_id
                || next.source_content_hash != previous.source_content_hash
                || next.source_span != previous.source_span
                || next.exact_material != previous.exact_material
                || match previous.status {
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
                return AtomicReviewTransition::Failed;
            }
        }
        let classified_manifest_revision = dossier.manifest_revision.saturating_add(1);
        let manifest_hash = requirement_manifest_hash(classified_manifest_revision, &requirements);
        self.atomic_review_update(dossier.document_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            ledger
                .mapping_revisions
                .extend(
                    mappings
                        .into_iter()
                        .map(|(source_id, mapping)| SourceMappingRevision {
                            completion_epoch: ledger.completion_epoch,
                            manifest_revision: classified_manifest_revision,
                            source_id,
                            mapping,
                        }),
                );
            ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                completion_epoch: ledger.completion_epoch,
                manifest_revision: classified_manifest_revision,
                manifest_hash,
                requirements,
            });
            ledger.manifest_revision = classified_manifest_revision;
            if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                cycle.cycle_id = format!(
                    "cycle-{}-{classified_manifest_revision}",
                    ledger.completion_epoch
                );
                cycle.manifest_revision = classified_manifest_revision;
                cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
            }
            document.evidence_epoch = document.evidence_epoch.saturating_add(1);
            document.desktop_activation_receipt = None;
            document.completion = None;
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

    pub(crate) async fn completion_gate(&self) -> Option<TaskCompletionGate> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let source_capture_failed = self.user_source_capture_failed();
        let mut latest_gate = None;
        for _ in 0..8 {
            self.refresh_external_file_freshness().await;
            let (gate, snapshot) = self
                .update_document(|document| {
                    if !task_is_tracked(document) {
                        return None;
                    }
                    if self.evidence_path.is_some() {
                        resolve_risk(document, "task-evidence-storage-failure");
                    }
                    let mut gate = derive_completion_gate(document, self.evidence_path.as_deref());
                    overlay_completion_review_gate(document, &mut gate, source_capture_failed);
                    document.completion = Some(gate.clone());
                    document.updated_at = timestamp();
                    Some(gate)
                })
                .await?;
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
                    let current_gate =
                        derive_completion_gate(document, self.evidence_path.as_deref());
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
        if !self.allows_kd4_completion() {
            return;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let (expected, previous_artifacts, artifact_paths) = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| {
                    (
                        document.latest_file_hashes.clone(),
                        document.latest_generated_artifact_hashes.clone(),
                        document
                            .generated_artifact_requirements
                            .iter()
                            .filter_map(|requirement| requirement.path.clone())
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .unwrap_or_default()
        };
        if expected.is_empty() && artifact_paths.is_empty() {
            return;
        }
        let mut changed = Vec::new();
        for (path, previous) in expected {
            let current = snapshot_file(repo_root, &path).await;
            if current != previous {
                changed.push((previous, current));
            }
        }
        let mut current_artifacts = BTreeMap::new();
        let mut changed_artifacts = false;
        for path in artifact_paths {
            let current = snapshot_generated_artifact(repo_root, &path).await;
            changed_artifacts |= previous_artifacts.get(&path) != Some(&current);
            current_artifacts.insert(path, current);
        }
        changed_artifacts |= previous_artifacts.len() != current_artifacts.len();
        if changed.is_empty() && !changed_artifacts {
            return;
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
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
                    return;
                }
                let prior_artifact_state_exists = changed_artifacts
                    && !previous_artifacts.is_empty()
                    && artifact_state_is_current;
                let changed_files = !changed.is_empty();
                if changed_files || prior_artifact_state_exists {
                    invalidate_for_mutation(document);
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
                    for step in &mut document.plan {
                        if step.edit_paths.contains(&path)
                            && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                        {
                            step.status = StepStatus::Implemented;
                        }
                    }
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
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
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
        let cursor = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .and_then(|document| document.completion_review_v2.as_ref())
                .map(|ledger| ledger.last_workspace_event_epoch)
                .unwrap_or_default()
        };
        let scanned = events
            .iter()
            .filter(|event| event.epoch > cursor)
            .cloned()
            .collect::<Vec<_>>();
        if scanned.is_empty() {
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
            .filter(|event| match event.actor_kind {
                codex_agent_task_store::WorkspaceActorKind::Root => {
                    event.actor_id.as_deref() == Some(root_actor_id.as_str())
                }
                codex_agent_task_store::WorkspaceActorKind::Legacy => event
                    .actor_id
                    .as_deref()
                    .is_some_and(|actor_id| actor_id.starts_with(&legacy_actor_prefix)),
                codex_agent_task_store::WorkspaceActorKind::Typed
                | codex_agent_task_store::WorkspaceActorKind::External => false,
            })
            .cloned()
            .collect::<Vec<_>>();
        let unattributed_epochs = scanned
            .iter()
            .filter(|event| match event.actor_kind {
                codex_agent_task_store::WorkspaceActorKind::External => true,
                codex_agent_task_store::WorkspaceActorKind::Root => {
                    event.actor_id.as_deref() != Some(root_actor_id.as_str())
                }
                codex_agent_task_store::WorkspaceActorKind::Legacy => event
                    .actor_id
                    .as_deref()
                    .is_none_or(|actor_id| !actor_id.starts_with(&legacy_actor_prefix)),
                codex_agent_task_store::WorkspaceActorKind::Typed => event
                    .actor_id
                    .as_ref()
                    .is_none_or(|actor_id| !same_root_typed_actor_ids.contains(actor_id)),
            })
            .map(|event| event.epoch)
            .collect::<Vec<_>>();
        let mut snapshots = BTreeMap::new();
        let mut repository_wide = false;
        for event in &accepted {
            for path in &event.paths {
                if path == codex_agent_task_store::REPOSITORY_WIDE_PATH {
                    repository_wide = true;
                    continue;
                }
                snapshots
                    .entry(path.clone())
                    .or_insert(snapshot_file(repo_root, path).await);
            }
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                let Some(ledger) = document.completion_review_v2.as_mut() else {
                    return;
                };
                if ledger.last_workspace_event_epoch != cursor {
                    return;
                }
                ledger.last_workspace_event_epoch = max_epoch;
                if !accepted.is_empty() {
                    let has_unrepresented_mutation = repository_wide
                        || snapshots.iter().any(|(path, state)| {
                            document.latest_file_hashes.get(path) != Some(state)
                        });
                    if has_unrepresented_mutation {
                        invalidate_for_mutation(document);
                    }
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        return;
                    };
                    for event in &accepted {
                        let Some(actor_id) = event.actor_id.clone() else {
                            continue;
                        };
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
                        ledger
                            .attributed_workspace_events
                            .push(TaskAttributedWorkspaceEvent {
                                workspace_id: event.workspace_id.clone(),
                                epoch: event.epoch,
                                actor_id,
                                paths,
                            });
                    }
                    trim_to_last(
                        &mut ledger.attributed_workspace_events,
                        MAX_ATTRIBUTED_WORKSPACE_EVENTS,
                    );
                    for (path, state) in &snapshots {
                        document.latest_file_hashes.insert(path.clone(), state.clone());
                    }
                }
                if repository_wide || !unattributed_epochs.is_empty() {
                    let epoch = document.evidence_epoch;
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("unattributed-workspace-mutation-{epoch}"),
                            description: format!(
                                "concurrent or repository-wide mutation could not be attributed exactly (workspace epochs: {})",
                                unattributed_epochs
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
                document.completion = None;
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
                invalidate_for_mutation(document);
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
        let (task_epoch, step_id, workspace_root_fingerprint, host_mutation_revision) = {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return ExternalEvidenceCapture::Ignored;
            };
            (
                document.evidence_epoch,
                (self.mode == TaskEvidenceMode::Kd4Completion)
                    .then(|| document.active_step_id.clone())
                    .flatten(),
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
                document.external_evidence.push(ExternalEvidenceReceipt {
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
                });
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

fn material_for_span(source: &UserSourceRecord, span: &SourceSpan) -> Option<String> {
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

fn manifest_gap_additions(
    dossier: &CompletionReviewDossier,
    gaps: &[ManifestGapInput],
) -> Option<Vec<RequirementRecord>> {
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
    let mut seen = BTreeSet::new();
    let mut additions = Vec::new();
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
            let exact_material = material_for_span(source, span)?;
            if existing.contains(&reference) || !seen.insert(reference) {
                return None;
            }
            additions.push(RequirementRecord {
                requirement_id: deterministic_requirement_id(source, span),
                source_id: source.source_id.clone(),
                source_content_hash: source.content_hash.clone(),
                exact_material,
                source_span: span.clone(),
                status: RequirementStatus::Active,
                superseded_by: None,
            });
        }
    }
    Some(additions)
}

fn deterministic_requirement_id(source: &UserSourceRecord, span: &SourceSpan) -> String {
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

fn source_mappings_for(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
) -> BTreeMap<String, SourceMapping> {
    ledger
        .mapping_revisions
        .iter()
        .filter(|mapping| {
            mapping.completion_epoch == completion_epoch
                && mapping.manifest_revision == manifest_revision
        })
        .map(|mapping| (mapping.source_id.clone(), mapping.mapping.clone()))
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

fn desktop_activation_receipt_is_complete(_receipt: &DesktopActivationReceipt) -> bool {
    // Desktop activation proof remains planned until a host-owned, publish-ID-bound
    // initialization handshake exists. Legacy receipts came from command stdout and
    // must never satisfy the completion gate, even when loaded from persisted state.
    false
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
        let outcome = if last_revision > document.revision {
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
    if schema_version == TASK_EVIDENCE_SCHEMA_VERSION
        && let Err(reason) = validate_v5_completion_review(&document)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: format!("invalid V5 completion-review lineage: {reason}"),
        };
    }
    ExistingDocument::Loaded {
        document: Box::new(document),
        legacy_completion_model,
    }
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
                || finding.requirement_ids.is_empty()
                || finding
                    .requirement_ids
                    .iter()
                    .any(|requirement_id| !valid_requirement_ids.contains(requirement_id.as_str()))
                || !finding
                    .requirement_ids
                    .iter()
                    .any(|requirement_id| active_requirement_ids.contains(requirement_id.as_str()))
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
                    || receipt.repair_instruction_hash.is_some()
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
        findings: Vec::new(),
        dispositions: Vec::new(),
        manifest_gaps: Vec::new(),
        repair_instruction_hash: None,
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

fn step_materially_matches_item(step: &EvidencePlanStep, item: &PlanItemArg) -> bool {
    step.step == item.step
        && step.depends_on == item.depends_on
        && step.acceptance_criteria == item.acceptance_criteria
        && step.runtime_paths == item.runtime_paths
        && step.generated_artifacts == item.generated_artifacts
        && step.risks == item.risks
        && step.requires_desktop_activation == item.requires_desktop_activation
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
    }
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
            if reason == "no durable plan steps were recorded"
                || reason.starts_with("plan steps are not acknowledged as passed:")
                || (reason.starts_with("plan step `") && reason.contains("unfinished step"))
            {
                Some(format!(
                    "Complete the named durable plan obligation and attach its deterministic focused proof: {reason}"
                ))
            } else if reason == "required Desktop activation receipt is missing or stale" {
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
) -> TaskCompletionGate {
    let mut blocked = Vec::new();
    let mut partial = Vec::new();
    if document.plan.is_empty() {
        partial.push("no durable plan steps were recorded".to_string());
    }
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
                    || !desktop_activation_receipt_is_complete(receipt)
            })
    {
        partial.push("required Desktop activation receipt is missing or stale".to_string());
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
    for risk in document.risks.iter().filter(|risk| !risk.resolved) {
        if risk.blocking {
            blocked.push(risk.description.clone());
        } else if risk.source != "plan" {
            partial.push(risk.description.clone());
        }
    }
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
    let unresolved = source_capture_failed
        || ledger.review_risk.unresolved
        || cycle_phase.is_some_and(|phase| phase != CompletionReviewCyclePhase::Closed);
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
        gate.reasons.push(
            "completion review risk remains unresolved for the current candidate".to_string(),
        );
        gate.status = TaskCompletionStatus::Partial;
    }
    gate.reasons.sort();
    gate.reasons.dedup();
}

fn invalidate_for_mutation(document: &mut TaskEvidenceDocument) {
    document.host_mutation_revision = document.host_mutation_revision.saturating_add(1);
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
        if step.status == StepStatus::Passed {
            step.status = StepStatus::Implemented;
        }
    }
}

fn invalidate_for_plan_change(document: &mut TaskEvidenceDocument) {
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.desktop_activation_receipt = None;
    document.latest_generated_artifact_hashes.clear();
    document.completion = None;
}

fn task_is_tracked(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        || !document.edit_receipts.is_empty()
        || document
            .command_receipts
            .iter()
            .any(|receipt| receipt.possible_mutation)
        || document.risks.iter().any(|risk| {
            matches!(
                risk.source.as_str(),
                "task_evidence_storage" | "completion_review_recovery"
            ) && !risk.resolved
        })
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
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.unwrap_or(repo_root).join(path)
    };
    absolute
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .unwrap_or(absolute)
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

async fn snapshot_file(repo_root: &Path, normalized: &str) -> FileHashSnapshot {
    let path = Path::new(normalized);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    match sha1_file(&absolute).await {
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

async fn snapshot_generated_artifact(repo_root: &Path, normalized: &str) -> FileHashSnapshot {
    let normalized = normalize_slashes(normalized);
    let path = Path::new(&normalized);
    if path.is_absolute() {
        return rejected_generated_artifact_snapshot(&normalized, "AbsoluteArtifactPath");
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return rejected_generated_artifact_snapshot(&normalized, "PathTraversalOutsideRepository");
    }
    let absolute = repo_root.join(path);
    if !generated_artifact_path_is_contained(repo_root, &absolute) {
        return rejected_generated_artifact_snapshot(&normalized, "OutsideRepository");
    }
    snapshot_file(repo_root, &normalized).await
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
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
