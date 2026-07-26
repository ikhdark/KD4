use chrono::Utc;
use codex_git_utils::collect_git_info;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_tools::ToolName;
use codex_utils_path_uri::PathUri;
use codex_utils_stream_parser::extract_proposed_plan_text;
use codex_utils_stream_parser::strip_citations;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha1::Digest;
use sha1::Sha1;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::warn;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 12;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_VALIDATION_RECEIPTS: usize = 64;
const MAX_EXPLICIT_DIRECTORY_ENTRIES: usize = 100_000;
pub(crate) struct TaskEvidenceLedger {
    evidence_path: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    document: Mutex<Option<TaskEvidenceDocument>>,
    persistence: TaskEvidencePersistence,
    persistence_gate: Semaphore,
    last_persisted_revision: AtomicU64,
    active_mutations: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskEvidencePersistence {
    Disabled,
    InMemory,
    File,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskMutationGuard {
    _inner: Arc<TaskMutationGuardInner>,
}

#[derive(Debug)]
struct TaskMutationGuardInner {
    active_mutations: Arc<AtomicU64>,
    pre_handler_invalidation_started: AtomicBool,
}

impl Drop for TaskMutationGuardInner {
    fn drop(&mut self) {
        self.active_mutations.fetch_sub(1, Ordering::AcqRel);
    }
}

impl TaskMutationGuard {
    fn acquire(active_mutations: &Arc<AtomicU64>) -> Self {
        active_mutations.fetch_add(1, Ordering::AcqRel);
        Self {
            _inner: Arc::new(TaskMutationGuardInner {
                active_mutations: Arc::clone(active_mutations),
                pre_handler_invalidation_started: AtomicBool::new(false),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::acquire(&Arc::new(AtomicU64::new(0)))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskPhase {
    #[default]
    Unclassified,
    Investigating,
    Fixing,
    Closing,
    Reviewing,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskOutcome {
    Passed,
    Partial,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskClassification {
    #[serde(default)]
    pub exhaustive: bool,
    #[serde(default)]
    pub risk_domains: BTreeSet<String>,
    #[serde(default)]
    pub supported_non_git_roots: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InvestigationCheckpoint {
    pub summary: String,
    #[serde(default)]
    pub paths_reviewed: BTreeSet<String>,
    #[serde(default)]
    pub competing_paths_checked: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClosureSubmission {
    #[serde(default)]
    pub path_review: BTreeSet<String>,
    #[serde(default)]
    pub competing_paths_checked: BTreeSet<String>,
    #[serde(default)]
    pub validation_receipt_ids: BTreeSet<String>,
    #[serde(default)]
    pub runtime_evidence: BTreeSet<String>,
    #[serde(default)]
    pub missing_requirement_ids: BTreeSet<String>,
    #[serde(default)]
    pub actionable_findings: BTreeSet<String>,
    #[serde(default)]
    pub blocked_reasons: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskLifecycleStatus {
    pub phase: TaskPhase,
    pub outcome: Option<TaskOutcome>,
    pub mutation_revision: u64,
    pub accepted_evidence_revision: u64,
    pub review_required: bool,
    pub closure_fingerprint: Option<String>,
    pub incomplete_occurrences: u8,
    pub known_roots: Vec<String>,
    pub unsupported_mutation_targets: Vec<String>,
    pub validation_receipt_ids: Vec<String>,
    pub command_receipt_ids: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskContractUpdate {
    Extended,
    FinalCommitted,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct RepositoryState {
    root: String,
    #[serde(default)]
    available: bool,
    #[serde(default)]
    error: Option<String>,
    head: Option<String>,
    tracked_diff_hash: String,
    staged_diff_hash: String,
    untracked_hash: String,
    #[serde(default)]
    dirty_paths: BTreeSet<String>,
    #[serde(default)]
    dirty_file_snapshots: BTreeMap<String, FileHashSnapshot>,
    #[serde(default)]
    dirty_path_states: BTreeMap<String, GitDirtyPathState>,
    #[serde(default)]
    explicit_target_snapshots: BTreeMap<String, ExplicitMutationTargetState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GitDirtyPathState {
    #[serde(default)]
    status: String,
    #[serde(default)]
    index_hash: String,
    #[serde(default)]
    worktree: ExplicitMutationTargetState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ExplicitMutationTargetState {
    #[serde(default)]
    owner_root: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    exists: bool,
    #[serde(default)]
    read_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DescendantTaskEvidenceCoverage {
    thread_id: String,
    mutation_revision: u64,
    digest: String,
    known_roots: BTreeSet<String>,
    root_baselines: BTreeMap<String, RepositoryState>,
    observed_roots: BTreeSet<String>,
    unsupported_mutation_targets: BTreeSet<String>,
    pass_prohibited_mutation_targets: BTreeSet<String>,
    mutation_targets: BTreeMap<String, String>,
    non_git_root_snapshots: BTreeMap<String, ExplicitMutationTargetState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AcceptedClosure {
    #[serde(default)]
    task_generation: u64,
    #[serde(default)]
    task_contract_hash: String,
    receipt_hash: String,
    mutation_revision: u64,
    accepted_evidence_revision: u64,
    frozen_diff_hash: String,
    #[serde(default)]
    terminal_outcome: Option<TaskOutcome>,
    #[serde(default)]
    missing_requirement_ids: BTreeSet<String>,
    #[serde(default)]
    validation_receipt_hashes: BTreeSet<String>,
    #[serde(default)]
    runtime_evidence_hashes: BTreeSet<String>,
    review_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PreparedReview {
    task_generation: u64,
    task_contract_hash: String,
    mutation_revision: u64,
    accepted_evidence_revision: u64,
    frozen_diff_hash: String,
    closure_receipt_hash: String,
    binding_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskEvidenceReviewPacket {
    pub prompt: String,
    pub binding_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskReviewReceipt {
    pub findings: BTreeSet<String>,
    pub verdict: String,
    pub explanation: String,
    pub confidence_score_millis: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommittedFinal {
    #[serde(default)]
    task_generation: u64,
    turn_id: String,
    item_id: String,
    evidence_revision: u64,
    #[serde(default)]
    emission_key: String,
    #[serde(default)]
    terminal_event: Option<EventMsg>,
    #[serde(default)]
    terminal_event_staged: bool,
    completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingFinalGate {
    turn_id: String,
    item_id: String,
    #[serde(default)]
    task_generation: u64,
    evidence_revision: u64,
    persisted: bool,
    #[serde(default)]
    history_position: Option<usize>,
    #[serde(default)]
    history_compacted: bool,
    #[serde(default)]
    emission_reserved: bool,
    externally_emitted: bool,
    #[serde(default)]
    externally_completed: bool,
    #[serde(default)]
    superseded: bool,
    #[serde(default)]
    response_item: Option<codex_protocol::models::ResponseItem>,
    #[serde(default)]
    emission_key: String,
    #[serde(default)]
    emission_items: Vec<TurnItem>,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoverableFinalEmission {
    pub(crate) turn_id: String,
    pub(crate) item_id: String,
    pub(crate) emission_key: String,
    pub(crate) items: Vec<TurnItem>,
    pub(crate) terminal_event: EventMsg,
    pub(crate) terminal_event_staged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManagedFinalState {
    NoFinalCandidate,
    AwaitingCommit,
    ItemsPending,
    TerminalPending,
    Completed,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskEvidenceValidationStart {
    epoch: u64,
    file_snapshots: BTreeMap<String, FileHashSnapshot>,
    artifact_snapshots: BTreeMap<String, FileHashSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistOutcome {
    Persisted,
    Superseded,
    Failed,
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
    validation_receipts: Vec<ValidationReceipt>,
    generated_artifact_requirements: Vec<GeneratedArtifactRequirement>,
    generated_artifact_hashes: BTreeMap<String, FileHashSnapshot>,
    #[serde(default)]
    latest_generated_artifact_hashes: BTreeMap<String, FileHashSnapshot>,
    latest_file_hashes: BTreeMap<String, FileHashSnapshot>,
    risks: Vec<EvidenceRisk>,
    verify_plan_epoch: Option<u64>,
    validation_epoch: Option<u64>,
    desktop_activation_receipt: Option<DesktopActivationReceipt>,
    #[serde(default)]
    automatic_plan_attempt_epoch: Option<u64>,
    repair_turns_used: u8,
    #[serde(default = "initial_receipt_sequence")]
    next_edit_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_command_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_validation_receipt_sequence: u64,
    completion: Option<TaskCompletionGate>,
    #[serde(default)]
    active_turn_id: Option<String>,
    #[serde(default)]
    task_contract: String,
    #[serde(default)]
    task_generation: u64,
    #[serde(default)]
    phase: TaskPhase,
    #[serde(default)]
    outcome: Option<TaskOutcome>,
    #[serde(default)]
    mutation_revision: u64,
    #[serde(default)]
    accepted_evidence_revision: u64,
    #[serde(default)]
    classification: Option<TaskClassification>,
    #[serde(default)]
    investigation_checkpoint_hash: Option<String>,
    #[serde(default)]
    known_roots: BTreeMap<String, RepositoryState>,
    #[serde(default)]
    root_baselines: BTreeMap<String, RepositoryState>,
    #[serde(default)]
    task_changed_paths: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    observed_roots: BTreeSet<String>,
    #[serde(default)]
    supported_non_git_roots: BTreeSet<String>,
    #[serde(default)]
    non_git_root_snapshots: BTreeMap<String, ExplicitMutationTargetState>,
    #[serde(default)]
    unsupported_mutation_targets: BTreeSet<String>,
    #[serde(default)]
    pass_prohibited_mutation_targets: BTreeSet<String>,
    #[serde(default)]
    mutation_targets: BTreeMap<String, String>,
    #[serde(default)]
    descendant_evidence_hashes: BTreeMap<String, String>,
    #[serde(default)]
    accepted_receipt_hashes: BTreeSet<String>,
    #[serde(default)]
    accepted_closure: Option<AcceptedClosure>,
    #[serde(default)]
    review_findings: BTreeSet<String>,
    #[serde(default)]
    latest_review_finding_revision: Option<u64>,
    #[serde(default)]
    actionable_findings: BTreeSet<String>,
    #[serde(default)]
    latest_actionable_finding_revision: Option<u64>,
    #[serde(default)]
    clean_review_hash: Option<String>,
    #[serde(default)]
    prepared_review: Option<PreparedReview>,
    #[serde(default)]
    review_attempt_failures: BTreeMap<String, u8>,
    #[serde(default)]
    closure_fingerprint: Option<String>,
    #[serde(default)]
    incomplete_occurrences: BTreeMap<String, u8>,
    #[serde(default)]
    pending_finals: Vec<PendingFinalGate>,
    #[serde(default)]
    final_emission_committed: bool,
    #[serde(default)]
    committed_final: Option<CommittedFinal>,
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
    validation_receipt_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EditIntent {
    call_id: String,
    step_id: Option<String>,
    started_at: String,
    completed_at: Option<String>,
    outcome: Option<String>,
    files: Vec<FileHashSnapshot>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidationReceipt {
    id: String,
    recorded_at: String,
    epoch: u64,
    step_id: Option<String>,
    mode: String,
    verdict: Option<String>,
    tool_success: bool,
    proof_bearing: bool,
    #[serde(default)]
    accepted_proof: bool,
    active_files: Vec<FileHashSnapshot>,
    stale_reasons: Vec<String>,
    payload: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GeneratedArtifactRequirement {
    id: String,
    step_id: Option<String>,
    path: Option<String>,
    validation_command: Vec<String>,
    source: String,
    #[serde(default)]
    validation_receipt_ids: Vec<String>,
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
    recorded_at: String,
    process_path: String,
    binary_sha1: String,
    runtime_evidence: String,
}

impl TaskEvidenceLedger {
    pub(crate) async fn load_or_new(codex_home: PathBuf, thread_id: ThreadId, cwd: &Path) -> Self {
        let repo_root = find_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let repo_is_git = repo_root.join(".git").exists();
        let evidence_path = codex_home
            .join("task-evidence")
            .join(format!("{thread_id}.json"));
        let now = timestamp();
        let thread_id_text = thread_id.to_string();
        let repository_root = repo_root.to_string_lossy().into_owned();

        let existing =
            load_existing_document(&evidence_path, &thread_id_text, &repository_root).await;
        let mut storage_failure_reason = None;
        let (existing, repository_changed) = match existing {
            ExistingDocument::Loaded {
                document,
                repository_changed,
            } => (Some(*document), repository_changed),
            ExistingDocument::Missing => (None, false),
            ExistingDocument::Rejected { kind, reason } => {
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
                (None, false)
            }
        };
        let document = if let Some(mut document) = existing {
            let previous_schema_version = document.schema_version;
            migrate_document(&mut document);
            if repository_changed {
                let fresh = new_task_evidence_document(
                    thread_id_text.clone(),
                    cwd,
                    &repo_root,
                    repo_is_git,
                    /*storage_failure_reason*/ None,
                    now.clone(),
                )
                .await;
                rebase_final_fence_for_repository_change(fresh, document)
            } else {
                // Repository snapshot representation last changed in v9. Later
                // final-outbox migrations must not replace a live task's mutation
                // baselines with its current state.
                if previous_schema_version < 9 {
                    let mut roots = document
                        .known_roots
                        .keys()
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    if repo_is_git {
                        roots.insert(repository_root.clone());
                    }
                    let roots = roots.into_iter().collect::<Vec<_>>();
                    let current_roots = snapshot_roots(&roots).await;
                    if document.mutation_revision > 0 {
                        document.root_baselines = current_roots
                            .keys()
                            .map(|root| {
                                (root.clone(), unknown_repository_baseline(Path::new(root)))
                            })
                            .collect();
                        document.task_changed_paths = current_roots
                            .keys()
                            .cloned()
                            .map(|root| (root, BTreeSet::from([".".to_string()])))
                            .collect();
                    } else {
                        document.root_baselines = current_roots.clone();
                        document.task_changed_paths.clear();
                    }
                    document.known_roots = current_roots;
                }
                if previous_schema_version < 12 {
                    document.non_git_root_snapshots =
                        snapshot_non_git_roots(&document.supported_non_git_roots).await;
                }
                document.updated_at = now.clone();
                document.revision = document.revision.saturating_add(1);
                document
            }
        } else {
            new_task_evidence_document(
                thread_id_text,
                cwd,
                &repo_root,
                repo_is_git,
                storage_failure_reason.as_deref(),
                now,
            )
            .await
        };
        let writable_evidence_path = storage_failure_reason.is_none().then_some(evidence_path);
        let ledger = Self {
            evidence_path: writable_evidence_path,
            repo_root: Some(repo_root),
            document: Mutex::new(Some(document.clone())),
            persistence: if storage_failure_reason.is_none() {
                TaskEvidencePersistence::File
            } else {
                TaskEvidencePersistence::Disabled
            },
            persistence_gate: Semaphore::new(1),
            last_persisted_revision: AtomicU64::new(0),
            active_mutations: Arc::new(AtomicU64::new(0)),
        };
        if storage_failure_reason.is_none() {
            let _ = ledger.persist_document(&document).await;
        }
        ledger
    }

    pub(crate) async fn in_memory(thread_id: ThreadId, cwd: &Path) -> Self {
        let repo_root = find_git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let repo_is_git = repo_root.join(".git").exists();
        let document = new_task_evidence_document(
            thread_id.to_string(),
            cwd,
            &repo_root,
            repo_is_git,
            /*storage_failure_reason*/ None,
            timestamp(),
        )
        .await;
        Self {
            evidence_path: None,
            repo_root: Some(repo_root),
            document: Mutex::new(Some(document)),
            persistence: TaskEvidencePersistence::InMemory,
            persistence_gate: Semaphore::new(1),
            last_persisted_revision: AtomicU64::new(0),
            active_mutations: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn descendant_coverage(
        &self,
    ) -> Result<DescendantTaskEvidenceCoverage, String> {
        let guard = self.document.lock().await;
        let document = guard
            .as_ref()
            .ok_or_else(|| "descendant task evidence is disabled".to_string())?;
        descendant_coverage_from_document(document)
    }

    pub(crate) async fn load_descendant_coverage(
        codex_home: &Path,
        thread_id: ThreadId,
    ) -> Result<DescendantTaskEvidenceCoverage, String> {
        let evidence_path = codex_home
            .join("task-evidence")
            .join(format!("{thread_id}.json"));
        let bytes = tokio::fs::read(&evidence_path).await.map_err(|err| {
            format!(
                "could not read descendant task evidence {}: {err}",
                evidence_path.display()
            )
        })?;
        let mut document =
            serde_json::from_slice::<TaskEvidenceDocument>(&bytes).map_err(|err| {
                format!(
                    "could not parse descendant task evidence {}: {err}",
                    evidence_path.display()
                )
            })?;
        if !(1..=TASK_EVIDENCE_SCHEMA_VERSION).contains(&document.schema_version) {
            return Err(format!(
                "descendant task evidence {} has unsupported schema version {}",
                evidence_path.display(),
                document.schema_version
            ));
        }
        if document.thread_id != thread_id.to_string() {
            return Err(format!(
                "descendant task evidence {} belongs to thread {} instead of {thread_id}",
                evidence_path.display(),
                document.thread_id
            ));
        }
        let previous_schema_version = document.schema_version;
        migrate_document(&mut document);
        if previous_schema_version < 12 {
            document.non_git_root_snapshots =
                snapshot_non_git_roots(&document.supported_non_git_roots).await;
        }
        descendant_coverage_from_document(&document)
    }

    pub(crate) async fn merge_descendant_coverage(
        &self,
        coverages: &[DescendantTaskEvidenceCoverage],
    ) -> Result<(), String> {
        if coverages.is_empty() {
            return Ok(());
        }

        let existing_baselines = {
            let guard = self.document.lock().await;
            let document = guard
                .as_ref()
                .ok_or_else(|| "task evidence is disabled".to_string())?;
            document.root_baselines.clone()
        };
        let mut root_names = BTreeSet::new();
        for coverage in coverages {
            root_names.extend(coverage.known_roots.iter().cloned());
        }
        let non_git_root_names = coverages
            .iter()
            .flat_map(|coverage| coverage.non_git_root_snapshots.keys().cloned())
            .collect::<BTreeSet<_>>();
        for root in &root_names {
            let canonical = canonicalize_existing_path(Path::new(root))?;
            let owning_root = find_git_repo_root(&canonical).ok_or_else(|| {
                format!("descendant mutation root is no longer Git-owned: {root}")
            })?;
            let canonical_owner = canonicalize_existing_path(&owning_root)?;
            if canonical_owner != canonical || canonical.to_string_lossy() != root.as_str() {
                return Err(format!(
                    "descendant mutation root identity changed before parent closure: {root}"
                ));
            }
        }

        let current_roots = snapshot_roots(&root_names.iter().cloned().collect::<Vec<_>>()).await;
        let current_non_git_roots = snapshot_non_git_roots(&non_git_root_names).await;
        let mut selected_new_baselines = BTreeMap::new();
        for (root, current) in &current_roots {
            if existing_baselines.contains_key(root) {
                continue;
            }
            let mut candidates = coverages
                .iter()
                .filter_map(|coverage| coverage.root_baselines.get(root))
                .cloned()
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                candidates.push(current.clone());
            }
            let mut selected = None::<(usize, String, RepositoryState)>;
            for candidate in candidates {
                let changed =
                    task_changed_paths_from_baseline(Path::new(root), Some(&candidate), current)
                        .await;
                let score = (changed.len(), canonical_hash(&candidate));
                if selected
                    .as_ref()
                    .is_none_or(|best| (score.0, &score.1) > (best.0, &best.1))
                {
                    selected = Some((score.0, score.1, candidate));
                }
            }
            if let Some((_, _, baseline)) = selected {
                selected_new_baselines.insert(root.clone(), baseline);
            }
        }

        let Some((changed, snapshot)) = self
            .update_document(|document| {
                let mut changed = false;
                for coverage in coverages {
                    let previous = document
                        .descendant_evidence_hashes
                        .insert(coverage.thread_id.clone(), coverage.digest.clone());
                    if coverage.mutation_revision > 0
                        && previous.as_deref() != Some(coverage.digest.as_str())
                    {
                        changed = true;
                    }
                    let unsupported_before = document.unsupported_mutation_targets.len();
                    document
                        .unsupported_mutation_targets
                        .extend(coverage.unsupported_mutation_targets.iter().cloned());
                    changed |= document.unsupported_mutation_targets.len() != unsupported_before;
                    let pass_prohibited_before = document.pass_prohibited_mutation_targets.len();
                    document
                        .pass_prohibited_mutation_targets
                        .extend(coverage.pass_prohibited_mutation_targets.iter().cloned());
                    changed |=
                        document.pass_prohibited_mutation_targets.len() != pass_prohibited_before;
                    for (target, root) in &coverage.mutation_targets {
                        if document.mutation_targets.get(target) != Some(root) {
                            document
                                .mutation_targets
                                .insert(target.clone(), root.clone());
                            changed = true;
                        }
                    }
                    document
                        .observed_roots
                        .extend(coverage.observed_roots.iter().cloned());
                }
                for (root, state) in current_roots {
                    if !document.root_baselines.contains_key(&root)
                        && let Some(baseline) = selected_new_baselines.get(&root)
                    {
                        document
                            .root_baselines
                            .insert(root.clone(), baseline.clone());
                    }
                    if document.known_roots.get(&root) != Some(&state) {
                        document.observed_roots.insert(root.clone());
                        document.known_roots.insert(root, state);
                        changed = true;
                    }
                }
                for (root, state) in current_non_git_roots {
                    if document.non_git_root_snapshots.get(&root) != Some(&state) {
                        document.non_git_root_snapshots.insert(root, state);
                        changed = true;
                    }
                }
                if changed {
                    invalidate_for_mutation(document);
                }
                changed
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        if changed {
            self.persist_required(&snapshot, "merging descendant mutation evidence")
                .await?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn disabled() -> Self {
        Self {
            evidence_path: None,
            repo_root: None,
            document: Mutex::new(None),
            persistence: TaskEvidencePersistence::Disabled,
            persistence_gate: Semaphore::new(1),
            last_persisted_revision: AtomicU64::new(0),
            active_mutations: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn matches_repo_root(&self, candidate: &Path) -> bool {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        let Ok(repo_root) = std::fs::canonicalize(repo_root) else {
            return false;
        };
        let Ok(candidate) = std::fs::canonicalize(candidate) else {
            return false;
        };
        repo_root == candidate
    }

    pub(crate) async fn begin_turn(
        &self,
        turn_id: &str,
        task_contract: &str,
    ) -> Result<(), String> {
        let root_state = match self.repo_root.as_ref() {
            Some(root) if root.join(".git").exists() => Some(snapshot_repository_state(root).await),
            _ => None,
        };
        let Some((changed, snapshot)) = self
            .update_document(|document| {
                if document.active_turn_id.as_deref() == Some(turn_id) {
                    return Ok(false);
                }
                if self.active_mutations.load(Ordering::Acquire) != 0 {
                    return Err(
                        "cannot begin a different task turn while task mutations are still active"
                            .to_string(),
                    );
                }
                let task_contract = task_contract.trim();
                let contract_changed =
                    !task_contract.is_empty() && document.task_contract != task_contract;
                if document
                    .committed_final
                    .as_ref()
                    .is_some_and(|committed| !committed.completed)
                {
                    return Err(
                        "cannot begin a different task turn while final emission is incomplete"
                            .to_string(),
                    );
                }
                let can_reset_completed_task = document
                    .committed_final
                    .as_ref()
                    .is_some_and(|committed| committed.completed);
                let is_pristine_initial_state = document.active_turn_id.is_none()
                    && document.mutation_revision == 0
                    && document.classification.is_none();
                if can_reset_completed_task || is_pristine_initial_state {
                    reset_task_document(document, root_state);
                    document.task_generation = document.task_generation.saturating_add(1);
                } else if contract_changed || document.active_turn_id.is_none() {
                    supersede_uncommitted_task(document);
                    document.task_generation = document.task_generation.saturating_add(1);
                }
                document.active_turn_id = Some(turn_id.to_string());
                if !task_contract.is_empty() {
                    document.task_contract = task_contract.to_string();
                }
                document.updated_at = timestamp();
                Ok(true)
            })
            .await
        else {
            return Ok(());
        };
        let changed = changed?;
        if changed {
            self.persist_required(&snapshot, "starting the task turn")
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn extend_task_contract(
        &self,
        turn_id: &str,
        task_contract_fragment: &str,
    ) -> Result<TaskContractUpdate, String> {
        let task_contract_fragment = task_contract_fragment.trim();
        if task_contract_fragment.is_empty() {
            return Ok(TaskContractUpdate::Extended);
        }
        let mut guard = self.document.lock().await;
        let Some(document) = guard.as_mut() else {
            return Ok(TaskContractUpdate::Extended);
        };
        if document.active_turn_id.as_deref() != Some(turn_id) {
            return Err("task contract update did not match the active task turn".to_string());
        }
        if document.final_emission_committed {
            return Ok(TaskContractUpdate::FinalCommitted);
        }
        if !document.task_contract.is_empty() {
            document.task_contract.push('\n');
        }
        document.task_contract.push_str(task_contract_fragment);
        invalidate_for_scope_change(document);
        document.revision = document.revision.saturating_add(1);
        document.updated_at = timestamp();
        let snapshot = document.clone();
        drop(guard);
        self.persist_required(&snapshot, "extending the active task contract")
            .await?;
        Ok(TaskContractUpdate::Extended)
    }

    pub(crate) async fn classify(
        &self,
        classification: TaskClassification,
    ) -> Result<TaskLifecycleStatus, String> {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return Err("task evidence is unavailable".to_string());
        };
        let classification = normalize_classification(classification, repo_root)?;
        let active_mutations = Arc::clone(&self.active_mutations);
        let captured = {
            let guard = self.document.lock().await;
            let document = guard
                .as_ref()
                .ok_or_else(|| "task evidence is disabled".to_string())?;
            if active_mutations.load(Ordering::Acquire) != 0 {
                return Err(
                    "task classification cannot change while mutation is in flight".to_string(),
                );
            }
            (document.revision, document.mutation_revision)
        };
        let root_state = if repo_root.join(".git").exists() {
            Some(snapshot_repository_state(repo_root).await)
        } else {
            None
        };
        let non_git_root_snapshots =
            snapshot_non_git_roots(&classification.supported_non_git_roots).await;
        if let Some((root, _state)) = non_git_root_snapshots
            .iter()
            .find(|(_, state)| state.owner_root.is_some())
        {
            return Err(format!(
                "supported non-Git root `{root}` became Git-owned during classification"
            ));
        }
        let Some((status, snapshot)) = self
            .update_document(|document| {
                if document.revision != captured.0
                    || document.mutation_revision != captured.1
                    || active_mutations.load(Ordering::Acquire) != 0
                {
                    return Err(
                        "task state changed while classification roots were frozen; retry classification"
                            .to_string(),
                    );
                }
                if document.final_emission_committed {
                    return Err(
                        "task classification cannot change after final output committed"
                            .to_string(),
                    );
                }
                if let Some(existing) = document.classification.clone() {
                    if existing.supported_non_git_roots
                        != classification.supported_non_git_roots
                        || (existing.exhaustive && !classification.exhaustive)
                        || !existing
                            .risk_domains
                            .is_subset(&classification.risk_domains)
                    {
                        return Err(
                            "task classification may only escalate exhaustive or review-risk flags; mutation roots are fixed"
                            .to_string(),
                        );
                    }
                    let root_drifted = root_state.as_ref().is_some_and(|state| {
                        document.known_roots.get(&state.root) != Some(state)
                    });
                    let non_git_drifted =
                        document.non_git_root_snapshots != non_git_root_snapshots;
                    if let Some(root_state) = root_state {
                        document
                            .known_roots
                            .insert(root_state.root.clone(), root_state);
                    }
                    document.non_git_root_snapshots = non_git_root_snapshots;
                    if canonical_hash(&existing) != canonical_hash(&classification) {
                        let requires_fresh_investigation = classification.exhaustive;
                        let starts_exhaustive_investigation =
                            !existing.exhaustive && classification.exhaustive;
                        document.classification = Some(classification);
                        if root_drifted || non_git_drifted {
                            invalidate_for_mutation(document);
                            if requires_fresh_investigation {
                                document.investigation_checkpoint_hash = None;
                                document.phase = TaskPhase::Investigating;
                            }
                        } else {
                            invalidate_for_scope_change(document);
                        }
                        if starts_exhaustive_investigation {
                            document.phase = TaskPhase::Investigating;
                            document.investigation_checkpoint_hash = None;
                        }
                        document.updated_at = timestamp();
                    } else if root_drifted || non_git_drifted {
                        invalidate_for_mutation(document);
                        document.updated_at = timestamp();
                    }
                    return Ok(status_from_document(
                        document,
                        "classification was accepted or monotonically escalated",
                    ));
                }
                let root_drifted = root_state
                    .as_ref()
                    .is_some_and(|state| document.known_roots.get(&state.root) != Some(state));
                let newly_observed_root = root_state
                    .as_ref()
                    .is_some_and(|state| !document.known_roots.contains_key(&state.root));
                document.supported_non_git_roots =
                    classification.supported_non_git_roots.clone();
                document.non_git_root_snapshots = non_git_root_snapshots;
                if let Some(root_state) = root_state {
                    if newly_observed_root {
                        document.observed_roots.insert(root_state.root.clone());
                    }
                    let baseline_if_missing = if root_drifted || document.mutation_revision > 0 {
                        unknown_repository_baseline(Path::new(&root_state.root))
                    } else {
                        root_state.clone()
                    };
                    document
                        .root_baselines
                        .entry(root_state.root.clone())
                        .or_insert(baseline_if_missing);
                    document
                        .known_roots
                        .insert(root_state.root.clone(), root_state);
                }
                if root_drifted {
                    invalidate_for_mutation(document);
                }
                document.phase = if classification.exhaustive {
                    TaskPhase::Investigating
                } else {
                    TaskPhase::Fixing
                };
                document.classification = Some(classification);
                document.outcome = None;
                document.updated_at = timestamp();
                Ok(status_from_document(document, "classification accepted"))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = status?;
        self.persist_required(&snapshot, "persisting task classification")
            .await?;
        Ok(status)
    }

    pub(crate) async fn submit_investigation_checkpoint(
        &self,
        checkpoint: InvestigationCheckpoint,
    ) -> Result<TaskLifecycleStatus, String> {
        let summary = checkpoint.summary.trim().to_string();
        if summary.is_empty() || checkpoint.paths_reviewed.is_empty() {
            return Err(
                "an investigation checkpoint requires a summary and at least one reviewed path"
                    .to_string(),
            );
        }
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((status, snapshot)) = self
            .update_document(|document| {
                if active_mutations.load(Ordering::Acquire) != 0 {
                    return Err(
                        "investigation checkpoint cannot change phase while mutation is in flight"
                            .to_string(),
                    );
                }
                if document.phase != TaskPhase::Investigating {
                    return Err(format!(
                        "investigation checkpoint is only valid while investigating (current phase: {:?})",
                        document.phase
                    ));
                }
                let paths_reviewed = normalize_review_paths(
                    document,
                    &checkpoint.paths_reviewed,
                    "investigation path review",
                )?;
                let competing_paths_checked = normalize_review_paths(
                    document,
                    &checkpoint.competing_paths_checked,
                    "investigation competing-path check",
                )?;
                let receipt_hash = canonical_hash(&serde_json::json!({
                    "summary": summary,
                    "paths_reviewed": paths_reviewed,
                    "competing_paths_checked": competing_paths_checked,
                }));
                if document.investigation_checkpoint_hash.as_deref() != Some(&receipt_hash) {
                    document.accepted_evidence_revision =
                        document.accepted_evidence_revision.saturating_add(1);
                    document.investigation_checkpoint_hash = Some(receipt_hash);
                }
                document.phase = TaskPhase::Fixing;
                document.updated_at = timestamp();
                Ok(status_from_document(
                    document,
                    "investigation checkpoint accepted; mutation is now available",
                ))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = status?;
        self.persist_required(&snapshot, "persisting investigation checkpoint")
            .await?;
        Ok(status)
    }

    pub(crate) async fn inspect_status(&self) -> Option<TaskLifecycleStatus> {
        let guard = self.document.lock().await;
        guard
            .as_ref()
            .map(|document| status_from_document(document, "current task lifecycle status"))
    }

    pub(crate) async fn managed_final_state_for_turn(
        &self,
        turn_id: &str,
    ) -> Option<ManagedFinalState> {
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        if document.active_turn_id.as_deref() != Some(turn_id) {
            return None;
        }
        let Some(committed) = document.committed_final.as_ref().filter(|committed| {
            committed.task_generation == document.task_generation
                && committed.turn_id == turn_id
                && committed.evidence_revision == document.accepted_evidence_revision
        }) else {
            let has_pending_candidate = document.pending_finals.iter().any(|pending| {
                pending.task_generation == document.task_generation
                    && pending.turn_id == turn_id
                    && !pending.superseded
                    && (pending.persisted || pending.emission_reserved)
            });
            return Some(if has_pending_candidate {
                ManagedFinalState::AwaitingCommit
            } else {
                ManagedFinalState::NoFinalCandidate
            });
        };
        if committed.completed {
            return Some(ManagedFinalState::Completed);
        }
        if exact_final_items_emitted(document, turn_id) {
            Some(ManagedFinalState::TerminalPending)
        } else {
            Some(ManagedFinalState::ItemsPending)
        }
    }

    pub(crate) async fn manages_turn(&self, turn_id: &str) -> bool {
        self.document
            .lock()
            .await
            .as_ref()
            .is_some_and(|document| document.active_turn_id.as_deref() == Some(turn_id))
    }

    pub(crate) async fn protected_pending_final_item_ids(&self) -> BTreeSet<String> {
        self.document
            .lock()
            .await
            .as_ref()
            .map(|document| {
                document
                    .pending_finals
                    .iter()
                    .filter(|pending| !(pending.externally_emitted && pending.externally_completed))
                    .map(|pending| pending.item_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn preflight_managed_mutation(&self, turn_id: &str) -> Result<(), String> {
        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return Ok(());
        };
        ensure_mutation_phase(document, turn_id)
    }

    pub(crate) async fn guard_tool_dispatch(
        &self,
        tool_name: &ToolName,
        turn_id: &str,
        declared_read_only: bool,
        trusted_external_read_only: bool,
        force_read_only: bool,
        trusted_mutator: bool,
        trusted_builtin: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        self.guard_tool_dispatch_mode(
            tool_name,
            turn_id,
            declared_read_only,
            trusted_external_read_only,
            force_read_only,
            trusted_mutator,
            trusted_builtin,
            true,
        )
        .await
    }

    pub(crate) async fn reserve_tool_dispatch(
        &self,
        tool_name: &ToolName,
        turn_id: &str,
        declared_read_only: bool,
        trusted_external_read_only: bool,
        force_read_only: bool,
        trusted_mutator: bool,
        trusted_builtin: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        self.guard_tool_dispatch_mode(
            tool_name,
            turn_id,
            declared_read_only,
            trusted_external_read_only,
            force_read_only,
            trusted_mutator,
            trusted_builtin,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn guard_tool_dispatch_mode(
        &self,
        tool_name: &ToolName,
        turn_id: &str,
        declared_read_only: bool,
        trusted_external_read_only: bool,
        force_read_only: bool,
        trusted_mutator: bool,
        trusted_builtin: bool,
        invalidate_before_handler: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        let managed_task = {
            let guard = self.document.lock().await;
            if let Some(document) = guard.as_ref() {
                if document.active_turn_id.as_deref() != Some(turn_id) {
                    return Err(
                        "tool rejected because its turn is no longer the active task turn"
                            .to_string(),
                    );
                }
                true
            } else {
                false
            }
        };
        let core_tool = tool_name.namespace.is_none();
        if force_read_only && core_tool && tool_name.name == "update_plan" {
            return Err(
                "mutation rejected: independent review delegates cannot update task state"
                    .to_string(),
            );
        }
        // External declarations are not sufficient on their own: the router must
        // also have conflict-closed provenance, and reviewers remain isolated.
        if (declared_read_only
            && (trusted_builtin || (trusted_external_read_only && !force_read_only)))
            || (trusted_builtin && core_tool && tool_is_lifecycle_or_read_only(&tool_name.name))
            || (trusted_builtin && core_tool && tool_is_normalized_shell(&tool_name.name))
        {
            return Ok(None);
        }
        if force_read_only {
            return Err(
                "mutation rejected: independent review delegates are read-only".to_string(),
            );
        }
        if !managed_task {
            return Ok(None);
        }
        if !(trusted_builtin && core_tool && tool_is_supported_local_mutator(&tool_name.name))
            && !trusted_mutator
        {
            return Err(format!(
                "mutation rejected: tool `{tool_name}` has no resolvable local mutation owner"
            ));
        }
        let active_mutations = Arc::clone(&self.active_mutations);
        if !invalidate_before_handler {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return Ok(None);
            };
            ensure_mutation_phase(document, turn_id)?;
            return Ok(Some(TaskMutationGuard::acquire(&active_mutations)));
        }
        let Some((result, snapshot)) = self
            .update_document(|document| {
                ensure_mutation_phase(document, turn_id)?;
                invalidate_for_mutation(document);
                Ok::<TaskMutationGuard, String>(TaskMutationGuard::acquire(&active_mutations))
            })
            .await
        else {
            return Ok(None);
        };
        let mutation_guard = result?;
        if let Err(err) = self
            .persist_required(&snapshot, "authorizing local mutation")
            .await
        {
            drop(mutation_guard);
            return Err(err);
        }
        Ok(Some(mutation_guard))
    }

    pub(crate) async fn finish_reserved_tool_dispatch(
        &self,
        turn_id: &str,
        reservation: &TaskMutationGuard,
    ) -> Result<bool, String> {
        if !Arc::ptr_eq(&reservation._inner.active_mutations, &self.active_mutations) {
            return Err("mutation reservation does not belong to this task ledger".to_string());
        }
        let invalidated_before_handler = reservation
            ._inner
            .pre_handler_invalidation_started
            .load(Ordering::Acquire);
        loop {
            let (
                snapshot_revision,
                current_roots,
                current_task_changed_paths,
                current_non_git_roots,
            ) = self.snapshot_known_roots_and_task_changes().await;
            let mut guard = self.document.lock().await;
            let Some(document) = guard.as_mut() else {
                return Ok(false);
            };
            if document.active_turn_id.as_deref() != Some(turn_id) {
                return Err(
                    "mutation result rejected because its turn is no longer the active task turn"
                        .to_string(),
                );
            }
            if document.revision != snapshot_revision {
                drop(guard);
                continue;
            }
            let root_drifted = current_roots != document.known_roots
                || current_non_git_roots != document.non_git_root_snapshots;
            if !root_drifted {
                return Ok(invalidated_before_handler);
            }
            apply_root_snapshots(document, current_roots, current_task_changed_paths);
            document.non_git_root_snapshots = current_non_git_roots;
            if !invalidated_before_handler {
                invalidate_for_mutation(document);
            }
            document.revision = document.revision.saturating_add(1);
            document.updated_at = timestamp();
            let snapshot = document.clone();
            drop(guard);
            self.persist_required(&snapshot, "committing tool mutation")
                .await?;
            return Ok(true);
        }
    }

    pub(crate) async fn start_reserved_tool_dispatch(
        &self,
        turn_id: &str,
        reservation: &TaskMutationGuard,
    ) -> Result<(), String> {
        if !Arc::ptr_eq(&reservation._inner.active_mutations, &self.active_mutations) {
            return Err("mutation reservation does not belong to this task ledger".to_string());
        }
        if reservation
            ._inner
            .pre_handler_invalidation_started
            .swap(true, Ordering::AcqRel)
        {
            return Err("mutation reservation was already started".to_string());
        }
        let Some((result, snapshot)) = self
            .update_document(|document| {
                ensure_mutation_phase(document, turn_id)?;
                invalidate_for_mutation(document);
                document.updated_at = timestamp();
                Ok::<(), String>(())
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        result?;
        self.persist_required(&snapshot, "starting reserved tool mutation")
            .await
    }

    pub(crate) async fn guard_normalized_command(
        &self,
        command: &[String],
        mutation_cwd: Option<&Path>,
        turn_id: &str,
        force_read_only: bool,
        has_unbounded_mutation_authority: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        let managed_task = self.document.lock().await.is_some();
        if command_is_proven_read_only(command) {
            return Ok(None);
        }
        if force_read_only {
            return Err(
                "command rejected: independent review delegates may run only proven read-only commands"
                    .to_string(),
            );
        }
        if !managed_task {
            return Ok(None);
        }
        if has_unbounded_mutation_authority {
            return Err(
                "command rejected: its effective permissions allow mutations outside bounded registered roots"
                    .to_string(),
            );
        }
        let Some(mutation_cwd) = mutation_cwd else {
            return Err(
                "command rejected: external mutation ownership is unresolvable".to_string(),
            );
        };
        self.preflight_managed_mutation(turn_id).await?;
        let mutation_targets = command_mutation_targets(command, mutation_cwd);
        self.register_mutation_targets(mutation_cwd, &mutation_targets.paths)
            .await?;
        let mutation_guard = self
            .guard_tool_dispatch(
                &ToolName::plain("normalized_shell_mutation"),
                turn_id,
                false,
                false,
                force_read_only,
                false,
                true,
            )
            .await?;
        Ok(mutation_guard)
    }

    pub(crate) async fn guard_active_turn_user_shell(
        &self,
        _turn_id: &str,
    ) -> Result<Option<TaskMutationGuard>, String> {
        if self.document.lock().await.is_none() {
            return Ok(None);
        }
        Err(
            "active-turn /shell is unavailable while task evidence is active because its full-access process tree cannot be strongly contained"
                .to_string(),
        )
    }

    pub(crate) async fn register_mutation_targets(
        &self,
        cwd: &Path,
        paths: &[PathBuf],
    ) -> Result<(), String> {
        let mut git_roots = BTreeSet::new();
        let mut non_git_targets = BTreeSet::new();
        let mut explicit_targets = Vec::new();
        let canonical_cwd = canonicalize_existing_path(cwd)?;
        if let Some(root) = find_git_repo_root(&canonical_cwd) {
            git_roots.insert(root);
        } else if paths.is_empty() {
            non_git_targets.insert(canonical_cwd);
        }
        for path in paths {
            let absolute = canonicalize_mutation_target(cwd, path)?;
            let target_state = snapshot_explicit_mutation_target(&absolute).await;
            if let Some(root) = target_state.owner_root.as_deref() {
                git_roots.insert(canonicalize_existing_path(Path::new(root))?);
            } else {
                non_git_targets.insert(absolute.clone());
            }
            explicit_targets.push((absolute.to_string_lossy().into_owned(), target_state));
        }
        let mut new_states = BTreeMap::new();
        let mut unavailable_roots = BTreeSet::new();
        for root in git_roots {
            let state = snapshot_repository_state(&root).await;
            if !state.available {
                unavailable_roots.insert(state.root.clone());
            }
            new_states.insert(state.root.clone(), state);
        }
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if !unavailable_roots.is_empty() {
                    document
                        .unsupported_mutation_targets
                        .extend(unavailable_roots.iter().cloned());
                    invalidate_for_plan_change(document);
                    return Err(format!(
                        "mutation target Git ownership is unavailable: {}",
                        unavailable_roots
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let mut ownership_changed = false;
                let mut observed_root_changed = false;
                let mut explicit_target_drifted = false;
                for target in &non_git_targets {
                    let supported = document
                        .supported_non_git_roots
                        .iter()
                        .any(|root| path_is_within(target, Path::new(root)));
                    if !supported {
                        document
                            .unsupported_mutation_targets
                            .insert(target.to_string_lossy().into_owned());
                        invalidate_for_plan_change(document);
                        return Err(format!(
                            "mutation target is not owned by a registered Git root: {}",
                            target.display()
                        ));
                    }
                    ownership_changed |= document
                        .pass_prohibited_mutation_targets
                        .insert(target.to_string_lossy().into_owned());
                }
                for (root, state) in new_states {
                    if !document.known_roots.contains_key(&root) {
                        observed_root_changed = true;
                        document.observed_roots.insert(root.clone());
                        document.root_baselines.insert(root.clone(), state.clone());
                        document.known_roots.insert(root, state);
                    }
                }
                for (target, target_state) in explicit_targets {
                    let Some(owner_root) = target_state.owner_root.as_ref() else {
                        continue;
                    };
                    let target_was_registered = document.mutation_targets.contains_key(&target);
                    let anchor_root = document
                        .mutation_targets
                        .get(&target)
                        .cloned()
                        .unwrap_or_else(|| owner_root.clone());
                    if !target_was_registered {
                        ownership_changed = true;
                        document
                            .mutation_targets
                            .insert(target.clone(), anchor_root.clone());
                        if let Some(baseline) = document.root_baselines.get_mut(&anchor_root) {
                            baseline
                                .explicit_target_snapshots
                                .insert(target.clone(), target_state.clone());
                        }
                    }
                    if let Some(current) = document.known_roots.get_mut(&anchor_root) {
                        if current.explicit_target_snapshots.get(&target) != Some(&target_state) {
                            if target_was_registered {
                                explicit_target_drifted = true;
                            } else {
                                ownership_changed = true;
                            }
                        }
                        current
                            .explicit_target_snapshots
                            .insert(target, target_state);
                    }
                }
                if observed_root_changed || explicit_target_drifted {
                    invalidate_for_mutation(document);
                } else if ownership_changed {
                    invalidate_for_plan_change(document);
                }
                Ok(())
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        self.persist_required(&snapshot, "registering mutation ownership")
            .await?;
        result
    }

    async fn reconcile_command_mutation_targets(
        &self,
        cwd: &Path,
        paths: &[PathBuf],
    ) -> Result<(), String> {
        let existing_roots = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| document.known_roots.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let mut explicit_targets = Vec::new();
        let mut current_root_states = BTreeMap::new();
        let mut unresolved_targets = BTreeSet::new();
        for path in paths {
            let absolute = canonicalize_mutation_target(cwd, path)?;
            let state = snapshot_explicit_mutation_target(&absolute).await;
            if let Some(owner_root) = state.owner_root.as_deref() {
                if !current_root_states.contains_key(owner_root) {
                    current_root_states.insert(
                        owner_root.to_string(),
                        snapshot_repository_state(Path::new(owner_root)).await,
                    );
                }
            } else {
                unresolved_targets.insert(absolute.to_string_lossy().into_owned());
            }
            explicit_targets.push((absolute.to_string_lossy().into_owned(), state));
        }
        let unavailable_roots = current_root_states
            .values()
            .filter(|state| !state.available)
            .map(|state| state.root.clone())
            .collect::<BTreeSet<_>>();
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if !unavailable_roots.is_empty() {
                    document
                        .unsupported_mutation_targets
                        .extend(unavailable_roots.iter().cloned());
                    return Err(format!(
                        "post-execution mutation target Git ownership is unavailable: {}",
                        unavailable_roots
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                for target in &unresolved_targets {
                    let supported = document
                        .supported_non_git_roots
                        .iter()
                        .any(|root| path_is_within(Path::new(target), Path::new(root)));
                    if supported {
                        document
                            .pass_prohibited_mutation_targets
                            .insert(target.clone());
                    } else {
                        document.unsupported_mutation_targets.insert(target.clone());
                        return Err(format!(
                            "post-execution mutation target has no supported owner: {target}"
                        ));
                    }
                }
                for (root, state) in current_root_states {
                    if !document.known_roots.contains_key(&root) {
                        document.observed_roots.insert(root.clone());
                        document
                            .root_baselines
                            .insert(root.clone(), absent_repository_state(Path::new(&root)));
                        document.known_roots.insert(root, state);
                    }
                }
                for (target, target_state) in explicit_targets {
                    let Some(current_owner) = target_state.owner_root.as_ref() else {
                        continue;
                    };
                    let anchor_root = document
                        .mutation_targets
                        .get(&target)
                        .cloned()
                        .or_else(|| deepest_containing_root(&target, &existing_roots))
                        .unwrap_or_else(|| current_owner.clone());
                    if !document.mutation_targets.contains_key(&target) {
                        document
                            .mutation_targets
                            .insert(target.clone(), anchor_root.clone());
                        if let Some(baseline) = document.root_baselines.get_mut(&anchor_root) {
                            let baseline_target = if Path::new(&target) == Path::new(&anchor_root) {
                                target_state.clone()
                            } else {
                                ExplicitMutationTargetState {
                                    owner_root: Some(anchor_root.clone()),
                                    kind: "absent".to_string(),
                                    sha1: None,
                                    exists: false,
                                    read_error: None,
                                }
                            };
                            baseline
                                .explicit_target_snapshots
                                .insert(target.clone(), baseline_target);
                        }
                    }
                    if let Some(current) = document.known_roots.get_mut(&anchor_root) {
                        current
                            .explicit_target_snapshots
                            .insert(target, target_state);
                    }
                }
                Ok(())
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        self.persist_required(&snapshot, "reconciling command mutation targets")
            .await?;
        result
    }

    pub(crate) async fn prepare_review(&self) -> Result<Option<TaskEvidenceReviewPacket>, String> {
        self.refresh_external_file_freshness().await?;
        let captured = {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return Err("task evidence is disabled".to_string());
            };
            if document.phase != TaskPhase::Reviewing {
                return Ok(None);
            }
            if self.active_mutations.load(Ordering::Acquire) != 0 {
                return Err(
                    "independent review cannot start while mutation is in flight".to_string(),
                );
            }
            (
                document.revision,
                document.task_generation,
                document.mutation_revision,
                document.accepted_evidence_revision,
                document.known_roots.keys().cloned().collect::<Vec<_>>(),
            )
        };
        let (snapshot_revision, current_roots, current_task_changed_paths, current_non_git_roots) =
            self.snapshot_known_roots_and_task_changes().await;
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if snapshot_revision != captured.0
                    || document.revision != captured.0
                    || document.task_generation != captured.1
                    || document.mutation_revision != captured.2
                    || document.accepted_evidence_revision != captured.3
                    || document.phase != TaskPhase::Reviewing
                    || active_mutations.load(Ordering::Acquire) != 0
                {
                    return Err(
                        "review state changed while the frozen repository was prepared".to_string(),
                    );
                }
                if current_roots != document.known_roots
                    || current_non_git_roots != document.non_git_root_snapshots
                {
                    document.known_roots = current_roots;
                    document.task_changed_paths = current_task_changed_paths;
                    document.non_git_root_snapshots = current_non_git_roots;
                    invalidate_for_mutation(document);
                    return Ok(None);
                }
                document.task_changed_paths = current_task_changed_paths;
                let Some(closure) = document.accepted_closure.clone() else {
                    document.phase = TaskPhase::Fixing;
                    document.outcome = None;
                    return Ok(None);
                };
                if closure.mutation_revision != document.mutation_revision
                    || closure.accepted_evidence_revision != document.accepted_evidence_revision
                    || closure.frozen_diff_hash != frozen_mutation_state_hash(document)
                    || closure.task_generation != document.task_generation
                    || closure.task_contract_hash
                        != sha1_hex(document.task_contract.as_bytes())
                {
                    document.phase = TaskPhase::Fixing;
                    document.outcome = None;
                    document.accepted_closure = None;
                    return Ok(None);
                }
                let task_contract_hash = sha1_hex(document.task_contract.as_bytes());
                let binding_hash = canonical_hash(&serde_json::json!({
                    "task_generation": document.task_generation,
                    "task_contract_hash": &task_contract_hash,
                    "mutation_revision": document.mutation_revision,
                    "accepted_evidence_revision": document.accepted_evidence_revision,
                    "frozen_diff_hash": &closure.frozen_diff_hash,
                    "closure_receipt_hash": &closure.receipt_hash,
                }));
                document.prepared_review = Some(PreparedReview {
                    task_generation: document.task_generation,
                    task_contract_hash,
                    mutation_revision: document.mutation_revision,
                    accepted_evidence_revision: document.accepted_evidence_revision,
                    frozen_diff_hash: closure.frozen_diff_hash.clone(),
                    closure_receipt_hash: closure.receipt_hash.clone(),
                    binding_hash: binding_hash.clone(),
                });
                let roots = serde_json::to_string_pretty(&document.known_roots)
                    .unwrap_or_else(|_| "{}".to_string());
                let baselines = serde_json::to_string_pretty(&document.root_baselines)
                    .unwrap_or_else(|_| "{}".to_string());
                let task_changed_paths =
                    serde_json::to_string_pretty(&document.task_changed_paths)
                        .unwrap_or_else(|_| "{}".to_string());
                let classification = serde_json::to_string_pretty(&document.classification)
                    .unwrap_or_else(|_| "null".to_string());
                Ok(Some(TaskEvidenceReviewPacket {
                    prompt: format!(
                        "Review the frozen task patch independently and read-only.\n\
                         Task contract:\n{}\n\
                         Classification:\n{}\n\
                         Registered root baselines:\n{}\n\
                         Frozen registered roots:\n{}\n\
                         Task-changed paths:\n{}\n\
                         Closure receipt hash: {}\n\
                         Return structured findings, an exact verdict (`patch is correct` only when clean), an explanation, and confidence.",
                        document.task_contract,
                        classification,
                        baselines,
                        roots,
                        task_changed_paths,
                        closure.receipt_hash,
                    ),
                    binding_hash,
                }))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let packet = result?;
        self.persist_required(&snapshot, "preparing independent review")
            .await?;
        Ok(packet)
    }

    pub(crate) async fn submit_closure(
        &self,
        submission: ClosureSubmission,
    ) -> Result<TaskLifecycleStatus, String> {
        self.refresh_external_file_freshness().await?;
        let blocked_reasons = normalize_blocked_reasons(&submission.blocked_reasons)?;
        let captured = {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return Err("task evidence is disabled".to_string());
            };
            if document.phase != TaskPhase::Fixing {
                return Err(format!(
                    "closure can only be submitted from Fixing (current phase: {:?})",
                    document.phase
                ));
            }
            if document.classification.is_none() {
                return Err("closure requires an accepted task classification".to_string());
            }
            if self.active_mutations.load(Ordering::Acquire) != 0 {
                return Err(
                    "closure rejected while an authorized mutation is still in flight".to_string(),
                );
            }
            (
                document.revision,
                document.task_generation,
                document.mutation_revision,
                document.accepted_evidence_revision,
                document.evidence_epoch,
                document.known_roots.keys().cloned().collect::<Vec<_>>(),
            )
        };
        let (snapshot_revision, current_roots, current_task_changed_paths, current_non_git_roots) =
            self.snapshot_known_roots_and_task_changes().await;
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if snapshot_revision != captured.0
                    || document.revision != captured.0
                    || document.task_generation != captured.1
                    || document.mutation_revision != captured.2
                    || document.accepted_evidence_revision != captured.3
                    || document.evidence_epoch != captured.4
                    || document.phase != TaskPhase::Fixing
                    || active_mutations.load(Ordering::Acquire) != 0
                {
                    return Err(
                        "closure state changed during root freezing; retry against the current revision"
                            .to_string(),
                    );
                }
                if current_roots.keys().ne(document.known_roots.keys()) {
                    let previous_roots = document.known_roots.keys().cloned().collect::<BTreeSet<_>>();
                    document.observed_roots.extend(
                        current_roots
                            .keys()
                            .filter(|root| !previous_roots.contains(*root))
                            .cloned(),
                    );
                    document.known_roots = current_roots;
                    document.task_changed_paths = current_task_changed_paths;
                    invalidate_for_mutation(document);
                    return Ok(status_from_document(
                        document,
                        "registered mutation root identity changed during closure; evidence was invalidated",
                    ));
                }
                if current_roots != document.known_roots {
                    document.known_roots = current_roots;
                    document.task_changed_paths = current_task_changed_paths;
                    invalidate_for_mutation(document);
                    return Ok(status_from_document(
                        document,
                        "repository drift invalidated closure before evidence acceptance; returned to Fixing",
                    ));
                }
                if current_non_git_roots != document.non_git_root_snapshots {
                    document.non_git_root_snapshots = current_non_git_roots;
                    document.task_changed_paths = current_task_changed_paths;
                    invalidate_for_mutation(document);
                    return Ok(status_from_document(
                        document,
                        "supported non-Git drift invalidated closure before evidence acceptance; returned to Fixing",
                    ));
                }
                document.task_changed_paths = current_task_changed_paths;
                if document.latest_review_finding_revision == Some(document.mutation_revision)
                    || document.latest_actionable_finding_revision
                        == Some(document.mutation_revision)
                {
                    document.phase = TaskPhase::Fixing;
                    document.outcome = None;
                    return Ok(status_from_document(
                        document,
                        "actionable findings remain unresolved at this mutation revision; mutate the repair before resubmitting closure",
                    ));
                }

                let validation_receipts = submission
                    .validation_receipt_ids
                    .iter()
                    .filter_map(|id| {
                        document
                            .validation_receipts
                            .iter()
                            .find(|receipt| {
                                &receipt.id == id
                                    && validation_receipt_is_accepted(
                                        receipt,
                                        document.evidence_epoch,
                                    )
                                    })
                    })
                    .collect::<Vec<_>>();
                let validation_receipts_complete =
                    validation_receipts.len() == submission.validation_receipt_ids.len();
                let validation_hashes = validation_receipts
                    .into_iter()
                    .map(canonical_validation_receipt_hash)
                    .collect::<BTreeSet<_>>();
                let runtime_receipts = submission
                    .runtime_evidence
                    .iter()
                    .filter_map(|id| {
                        document
                            .command_receipts
                            .iter()
                            .find(|receipt| {
                                &receipt.id == id
                                    && receipt.epoch == document.evidence_epoch
                                    && receipt.exit_code == 0
                                    && !receipt.timed_out
                                    && command_receipt_is_runtime_evidence(receipt, document)
                            })
                    })
                    .collect::<Vec<_>>();
                let runtime_receipts_complete =
                    runtime_receipts.len() == submission.runtime_evidence.len();
                let runtime_hashes = runtime_receipts
                    .into_iter()
                    .map(canonical_command_receipt_hash)
                    .collect::<BTreeSet<_>>();
                let path_review =
                    normalize_review_paths(document, &submission.path_review, "path review")?;
                let competing_paths_checked = normalize_review_paths(
                    document,
                    &submission.competing_paths_checked,
                    "competing-path check",
                )?;
                let missing_submitted =
                    normalize_stable_ids(&submission.missing_requirement_ids)?;
                let finding_ids = normalize_semantic_ids(&submission.actionable_findings);

                let mut missing = BTreeSet::new();
                if document.mutation_revision > 0 {
                    if path_review.is_empty() {
                        missing.insert("fresh-path-review".to_string());
                    }
                    if competing_paths_checked.is_empty() {
                        missing.insert("competing-path-check".to_string());
                    }
                    if !review_paths_cover_task_changes(
                        &path_review,
                        &document.task_changed_paths,
                    ) {
                        missing.insert(
                            "path-review-does-not-cover-task-changes".to_string(),
                        );
                    }
                    if validation_hashes.is_empty() && runtime_hashes.is_empty() {
                        missing.insert("runtime-or-wiring-evidence".to_string());
                    }
                    if !validation_receipts_complete {
                        missing.insert("unknown-or-stale-validation-receipt".to_string());
                    }
                    if !runtime_receipts_complete {
                        missing.insert("unknown-or-stale-runtime-receipt".to_string());
                    }
                }
                if !document.unsupported_mutation_targets.is_empty() {
                    missing.insert("supported-mutation-ownership".to_string());
                }
                if !document.pass_prohibited_mutation_targets.is_empty() {
                    missing.insert("git-backed-mutation-ownership".to_string());
                }
                let unavailable_root = document.known_roots.values().any(|state| !state.available);
                if unavailable_root {
                    missing.insert("registered-root-unavailable".to_string());
                }
                for reason in &blocked_reasons {
                    missing.insert(format!("blocked:{reason}"));
                }
                let (derived_missing, derived_actionable, derived_blocked) =
                    closure_runtime_requirements(document);
                missing.extend(derived_missing);
                if !missing_submitted.is_subset(&missing) {
                    return Err(format!(
                        "unknown missing requirement identifiers: {}",
                        missing_submitted
                            .difference(&missing)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let receipt_hash = canonical_hash(&serde_json::json!({
                    "path_review": path_review,
                    "competing_paths_checked": competing_paths_checked,
                    "validation_receipt_hashes": validation_hashes,
                    "runtime_evidence_hashes": runtime_hashes,
                    "actionable_finding_hashes": finding_ids.iter().map(|finding| sha1_hex(finding.as_bytes())).collect::<BTreeSet<_>>(),
                    "missing_requirement_ids": missing,
                    "blocked_reasons": blocked_reasons,
                }));
                document.phase = TaskPhase::Closing;
                let genuinely_new = document
                    .accepted_receipt_hashes
                    .insert(receipt_hash.clone());
                if genuinely_new {
                    document.accepted_evidence_revision =
                        document.accepted_evidence_revision.saturating_add(1);
                }

                if !finding_ids.is_empty() || !derived_actionable.is_empty() {
                    if !finding_ids.is_empty() {
                        document.actionable_findings.extend(finding_ids);
                        document.latest_actionable_finding_revision =
                            Some(document.mutation_revision);
                    }
                    document.actionable_findings.extend(derived_actionable);
                    document.phase = TaskPhase::Fixing;
                    document.outcome = None;
                    document.accepted_closure = None;
                    document.prepared_review = None;
                    document.closure_fingerprint = None;
                    return Ok(status_from_document(
                        document,
                        "actionable closure findings returned the task to Fixing",
                    ));
                }

                let frozen_diff_hash = frozen_mutation_state_hash(document);
                let fingerprint =
                    closure_fingerprint(document, &frozen_diff_hash, &missing, &validation_hashes);
                document.closure_fingerprint = Some(fingerprint.clone());

                if !missing.is_empty() {
                    let occurrence = document
                        .incomplete_occurrences
                        .entry(fingerprint)
                        .or_insert(0);
                    *occurrence = occurrence.saturating_add(1);
                    if *occurrence == 1 {
                        document.phase = TaskPhase::Fixing;
                        document.outcome = None;
                        return Ok(status_from_document(
                            document,
                            &format!(
                                "closure is incomplete; one recovery continuation is allowed: {}",
                                missing.iter().cloned().collect::<Vec<_>>().join(", ")
                            ),
                        ));
                    }
                    let useful_verified_work =
                        !validation_hashes.is_empty() || !runtime_hashes.is_empty();
                    let blocked = !useful_verified_work
                        || !blocked_reasons.is_empty()
                        || !document.unsupported_mutation_targets.is_empty()
                        || unavailable_root
                        || derived_blocked;
                    let outcome = if blocked {
                        TaskOutcome::Blocked
                    } else {
                        TaskOutcome::Partial
                    };
                    let review_required = review_required(document);
                    document.accepted_closure = Some(AcceptedClosure {
                        task_generation: document.task_generation,
                        task_contract_hash: sha1_hex(document.task_contract.as_bytes()),
                        receipt_hash,
                        mutation_revision: document.mutation_revision,
                        accepted_evidence_revision: document.accepted_evidence_revision,
                        frozen_diff_hash,
                        terminal_outcome: Some(outcome),
                        missing_requirement_ids: missing,
                        validation_receipt_hashes: validation_hashes,
                        runtime_evidence_hashes: runtime_hashes,
                        review_required,
                    });
                    if review_required {
                        document.phase = TaskPhase::Reviewing;
                        document.outcome = None;
                        document.prepared_review = None;
                        return Ok(status_from_document(
                            document,
                            "identical incomplete closure terminated deterministically; independent review is required",
                        ));
                    }
                    set_ready(document, outcome);
                    return Ok(status_from_document(
                        document,
                        "identical incomplete closure terminated deterministically",
                    ));
                }

                let review_required = review_required(document);
                document.accepted_closure = Some(AcceptedClosure {
                    task_generation: document.task_generation,
                    task_contract_hash: sha1_hex(document.task_contract.as_bytes()),
                    receipt_hash,
                    mutation_revision: document.mutation_revision,
                    accepted_evidence_revision: document.accepted_evidence_revision,
                    frozen_diff_hash,
                    terminal_outcome: Some(TaskOutcome::Passed),
                    missing_requirement_ids: BTreeSet::new(),
                    validation_receipt_hashes: validation_hashes,
                    runtime_evidence_hashes: runtime_hashes,
                    review_required,
                });
                if review_required {
                    document.phase = TaskPhase::Reviewing;
                    document.outcome = None;
                    document.prepared_review = None;
                    Ok(status_from_document(
                        document,
                        "closure accepted; independent review is required",
                    ))
                } else {
                    set_ready(document, TaskOutcome::Passed);
                    Ok(status_from_document(
                        document,
                        "fresh closure accepted; finalization is authorized",
                    ))
                }
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = result?;
        debug_assert_ne!(status.phase, TaskPhase::Closing);
        self.persist_required(&snapshot, "persisting atomic closure")
            .await?;
        Ok(status)
    }

    pub(crate) async fn accept_review(
        &self,
        binding_hash: &str,
        receipt: TaskReviewReceipt,
    ) -> Result<TaskLifecycleStatus, String> {
        self.refresh_external_file_freshness().await?;
        let confidence_valid = receipt.confidence_score_millis <= 1000;
        let (snapshot_revision, current_roots, current_task_changed_paths, current_non_git_roots) =
            self.snapshot_known_roots_and_task_changes().await;
        let findings = normalize_semantic_ids(&receipt.findings);
        let verdict = receipt.verdict.clone();
        let explanation = receipt.explanation.trim().to_string();
        let receipt_hash = canonical_hash(&serde_json::json!({
            "findings": findings,
            "verdict": verdict,
            "explanation": explanation,
            "confidence_score_millis": receipt.confidence_score_millis,
        }));
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if document.revision != snapshot_revision {
                    return Err(
                        "review state changed while reviewer roots were frozen; retry review"
                            .to_string(),
                    );
                }
                if document.phase != TaskPhase::Reviewing {
                    return Err(format!(
                        "review receipt is only valid in Reviewing (current phase: {:?})",
                        document.phase
                    ));
                }
                if active_mutations.load(Ordering::Acquire) != 0 {
                    return Err(
                        "review receipt rejected while mutation is still in flight".to_string(),
                    );
                }
                if current_roots != document.known_roots
                    || current_non_git_roots != document.non_git_root_snapshots
                {
                    document.known_roots = current_roots;
                    document.task_changed_paths = current_task_changed_paths;
                    document.non_git_root_snapshots = current_non_git_roots;
                    invalidate_for_mutation(document);
                    return Ok(status_from_document(
                        document,
                        "repository drift invalidated the reviewer receipt",
                    ));
                }
                document.task_changed_paths = current_task_changed_paths;
                let Some(closure) = document.accepted_closure.as_ref() else {
                    document.phase = TaskPhase::Fixing;
                    return Ok(status_from_document(
                        document,
                        "review had no matching accepted closure",
                    ));
                };
                let Some(prepared) = document.prepared_review.clone() else {
                    document.phase = TaskPhase::Fixing;
                    document.accepted_closure = None;
                    return Ok(status_from_document(
                        document,
                        "review receipt had no matching prepared review attempt",
                    ));
                };
                if closure.mutation_revision != document.mutation_revision
                    || closure.accepted_evidence_revision != document.accepted_evidence_revision
                    || closure.frozen_diff_hash != frozen_mutation_state_hash(document)
                    || closure.task_generation != document.task_generation
                    || closure.task_contract_hash
                        != sha1_hex(document.task_contract.as_bytes())
                    || prepared.mutation_revision != closure.mutation_revision
                    || prepared.accepted_evidence_revision
                        != closure.accepted_evidence_revision
                    || prepared.frozen_diff_hash != closure.frozen_diff_hash
                    || prepared.closure_receipt_hash != closure.receipt_hash
                    || prepared.task_generation != document.task_generation
                    || prepared.task_contract_hash
                        != sha1_hex(document.task_contract.as_bytes())
                    || prepared.binding_hash != binding_hash
                {
                    document.phase = TaskPhase::Fixing;
                    document.accepted_closure = None;
                    document.prepared_review = None;
                    return Ok(status_from_document(
                        document,
                        "reviewed repository state no longer matches closure",
                    ));
                }
                if !findings.is_empty() {
                    if document
                        .accepted_receipt_hashes
                        .insert(format!("review:{binding_hash}:{receipt_hash}"))
                    {
                        document.accepted_evidence_revision =
                            document.accepted_evidence_revision.saturating_add(1);
                    }
                    document.review_findings.extend(findings);
                    document.latest_review_finding_revision = Some(document.mutation_revision);
                    document.accepted_closure = None;
                    document.clean_review_hash = None;
                    document.prepared_review = None;
                    document.outcome = None;
                    document.phase = TaskPhase::Fixing;
                    return Ok(status_from_document(
                        document,
                        "review findings invalidated closure and returned the task to Fixing",
                    ));
                }
                if verdict != "patch is correct" || explanation.is_empty() || !confidence_valid {
                    let failure_fingerprint = prepared.binding_hash;
                    let occurrence = document
                        .review_attempt_failures
                        .entry(failure_fingerprint)
                        .or_insert(0);
                    *occurrence = occurrence.saturating_add(1);
                    document.prepared_review = None;
                    if *occurrence == 1 {
                        document.accepted_closure = None;
                        document.phase = TaskPhase::Fixing;
                        document.outcome = None;
                        return Ok(status_from_document(
                            document,
                            "review receipt was incomplete; one fresh closure/review retry is allowed",
                        ));
                    }
                    if let Some(closure) = document.accepted_closure.as_mut() {
                        closure.review_required = false;
                        closure.terminal_outcome = Some(TaskOutcome::Blocked);
                    }
                    set_ready(document, TaskOutcome::Blocked);
                    return Ok(status_from_document(
                        document,
                        "identical incomplete review terminated deterministically as Blocked",
                    ));
                }
                if document.clean_review_hash.as_deref() != Some(&receipt_hash) {
                    document.accepted_evidence_revision =
                        document.accepted_evidence_revision.saturating_add(1);
                    document.clean_review_hash = Some(receipt_hash);
                }
                if let Some(closure) = document.accepted_closure.as_mut() {
                    closure.accepted_evidence_revision = document.accepted_evidence_revision;
                }
                let reviewed_outcome = document
                    .accepted_closure
                    .as_ref()
                    .and_then(|closure| closure.terminal_outcome)
                    .unwrap_or(TaskOutcome::Blocked);
                document.prepared_review = None;
                set_ready(document, reviewed_outcome);
                Ok(status_from_document(
                    document,
                    "clean independent review authorized finalization",
                ))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = result?;
        self.persist_required(&snapshot, "accepting independent review")
            .await?;
        Ok(status)
    }

    pub(crate) async fn authorize_final_item(
        &self,
        turn_id: &str,
        item_id: &str,
    ) -> Result<bool, String> {
        self.refresh_external_file_freshness().await?;
        loop {
            let (
                snapshot_revision,
                current_roots,
                current_task_changed_paths,
                current_non_git_roots,
            ) = self.snapshot_known_roots_and_task_changes().await;
            let active_mutations = Arc::clone(&self.active_mutations);
            let update = self
                .update_document_if_revision(snapshot_revision, |document| {
                    if current_roots != document.known_roots
                        || current_non_git_roots != document.non_git_root_snapshots
                    {
                        document.known_roots = current_roots;
                        document.task_changed_paths = current_task_changed_paths;
                        document.non_git_root_snapshots = current_non_git_roots;
                        invalidate_for_mutation(document);
                        document.phase = TaskPhase::Fixing;
                        upsert_pending_final(
                            document,
                            PendingFinalGate {
                                turn_id: turn_id.to_string(),
                                item_id: item_id.to_string(),
                                task_generation: document.task_generation,
                                evidence_revision: document.accepted_evidence_revision,
                                persisted: false,
                                history_position: None,
                                history_compacted: false,
                                emission_reserved: false,
                                externally_emitted: false,
                                externally_completed: false,
                                superseded: false,
                                response_item: None,
                                emission_key: String::new(),
                                emission_items: Vec::new(),
                            },
                        );
                        return false;
                    }
                    document.task_changed_paths = current_task_changed_paths;
                    let non_mutating_final = document.mutation_revision == 0
                        && document.classification.is_none()
                        && document.unsupported_mutation_targets.is_empty()
                        && document.pass_prohibited_mutation_targets.is_empty()
                        && !matches!(
                            document.phase,
                            TaskPhase::Investigating | TaskPhase::Closing | TaskPhase::Reviewing
                        );
                    let eligible = document.active_turn_id.as_deref() == Some(turn_id)
                        && (ready_state_authorized(document) || non_mutating_final)
                        && active_mutations.load(Ordering::Acquire) == 0
                        && document.committed_final.is_none();
                    if eligible {
                        upsert_pending_final(
                            document,
                            PendingFinalGate {
                                turn_id: turn_id.to_string(),
                                item_id: item_id.to_string(),
                                task_generation: document.task_generation,
                                evidence_revision: document.accepted_evidence_revision,
                                persisted: false,
                                history_position: None,
                                history_compacted: false,
                                emission_reserved: true,
                                externally_emitted: false,
                                externally_completed: false,
                                superseded: false,
                                response_item: None,
                                emission_key: String::new(),
                                emission_items: Vec::new(),
                            },
                        );
                        true
                    } else {
                        upsert_pending_final(
                            document,
                            PendingFinalGate {
                                turn_id: turn_id.to_string(),
                                item_id: item_id.to_string(),
                                task_generation: document.task_generation,
                                evidence_revision: document.accepted_evidence_revision,
                                persisted: false,
                                history_position: None,
                                history_compacted: false,
                                emission_reserved: false,
                                externally_emitted: false,
                                externally_completed: false,
                                superseded: false,
                                response_item: None,
                                emission_key: String::new(),
                                emission_items: Vec::new(),
                            },
                        );
                        false
                    }
                })
                .await;
            let (result, snapshot) = match update {
                None => return Ok(true),
                Some(Err(())) => continue,
                Some(Ok(update)) => update,
            };
            self.persist_required(&snapshot, "reserving final output")
                .await?;
            return Ok(result);
        }
    }

    pub(crate) async fn commit_final_item(
        &self,
        turn_id: &str,
        item_id: &str,
    ) -> Result<bool, String> {
        self.refresh_external_file_freshness().await?;
        loop {
            let (
                snapshot_revision,
                current_roots,
                current_task_changed_paths,
                current_non_git_roots,
            ) = self.snapshot_known_roots_and_task_changes().await;
            let mut guard = self.document.lock().await;
            let Some(document) = guard.as_mut() else {
                return Ok(true);
            };
            if document.revision != snapshot_revision {
                drop(guard);
                continue;
            }
            if document.active_turn_id.as_deref() != Some(turn_id) {
                return Ok(false);
            }
            if self.active_mutations.load(Ordering::Acquire) != 0 {
                return Ok(false);
            }
            if current_roots != document.known_roots
                || current_non_git_roots != document.non_git_root_snapshots
            {
                document.known_roots = current_roots;
                document.task_changed_paths = current_task_changed_paths;
                document.non_git_root_snapshots = current_non_git_roots;
                invalidate_for_mutation(document);
                if let Some(pending) = document.pending_finals.iter_mut().find(|pending| {
                    pending.task_generation == document.task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                }) {
                    pending.emission_reserved = false;
                }
                document.revision = document.revision.saturating_add(1);
                let snapshot = document.clone();
                drop(guard);
                self.persist_required(&snapshot, "recording final-boundary drift")
                    .await?;
                return Ok(false);
            }
            document.task_changed_paths = current_task_changed_paths;
            let pending_index = document.pending_finals.iter().position(|pending| {
                pending.task_generation == document.task_generation
                    && pending.turn_id == turn_id
                    && pending.item_id == item_id
            });
            if let Some(index) = pending_index {
                let task_generation = document.task_generation;
                let evidence_revision = document.accepted_evidence_revision;
                hydrate_pending_final_emission(
                    &mut document.pending_finals[index],
                    task_generation,
                    evidence_revision,
                );
            }
            let eligible = pending_index.is_some_and(|index| {
                let pending = &document.pending_finals[index];
                pending.emission_reserved
                    && pending.persisted
                    && pending
                        .response_item
                        .as_ref()
                        .and_then(codex_protocol::models::ResponseItem::id)
                        == Some(item_id)
                    && !pending.superseded
                    && pending.evidence_revision == document.accepted_evidence_revision
                    && !pending.emission_key.is_empty()
                    && !pending.emission_items.is_empty()
            }) && (ready_state_authorized(document)
                || (document.mutation_revision == 0
                    && document.classification.is_none()
                    && document.unsupported_mutation_targets.is_empty()
                    && document.pass_prohibited_mutation_targets.is_empty()
                    && !matches!(
                        document.phase,
                        TaskPhase::Investigating | TaskPhase::Closing | TaskPhase::Reviewing
                    )));
            if !eligible {
                return Ok(false);
            }
            let emission_key = document.pending_finals
                [pending_index.expect("eligible pending final")]
            .emission_key
            .clone();
            let terminal_completion =
                ready_outcome_completion_gate(document, self.evidence_path.as_deref());
            let terminal_event = document.pending_finals
                [pending_index.expect("eligible pending final")]
            .emission_items
            .as_slice();
            let terminal_event =
                fallback_final_terminal_event(turn_id, terminal_event, terminal_completion);
            let precommit_document = document.clone();
            document.final_emission_committed = true;
            document.committed_final = Some(CommittedFinal {
                task_generation: document.task_generation,
                turn_id: turn_id.to_string(),
                item_id: item_id.to_string(),
                evidence_revision: document.accepted_evidence_revision,
                emission_key,
                terminal_event: Some(terminal_event),
                terminal_event_staged: false,
                completed: false,
            });
            let task_generation = document.task_generation;
            for pending in &mut document.pending_finals {
                if pending.task_generation == task_generation {
                    let selected = pending.turn_id == turn_id && pending.item_id == item_id;
                    pending.emission_reserved = false;
                    pending.externally_emitted = false;
                    pending.externally_completed = false;
                    if selected {
                        pending.superseded = false;
                    } else {
                        supersede_pending_final(pending);
                    }
                }
            }
            document.revision = document.revision.saturating_add(1);
            document.updated_at = timestamp();
            let snapshot = document.clone();
            return match self.persist_document(&snapshot).await {
                PersistOutcome::Persisted | PersistOutcome::Superseded => Ok(true),
                PersistOutcome::Failed => {
                    *document = precommit_document;
                    fail_closed_for_storage(document, "final emission commit was not durable");
                    document.revision = document.revision.saturating_add(1);
                    document.updated_at = timestamp();
                    let fail_closed_snapshot = document.clone();
                    let _ = self.persist_document(&fail_closed_snapshot).await;
                    Err(
                        "final emission denied because task evidence could not be persisted"
                            .to_string(),
                    )
                }
            };
        }
    }

    pub(crate) async fn stage_final_emission_items(
        &self,
        turn_id: &str,
        item_id: &str,
        items: &[TurnItem],
    ) -> Result<String, String> {
        let expected_plan_id = format!("{turn_id}-plan");
        let plan_item = match items {
            [TurnItem::AgentMessage(agent)] if agent.id == item_id => None,
            [TurnItem::Plan(plan)]
                if plan.id == expected_plan_id && !plan.text.trim().is_empty() =>
            {
                Some(plan)
            }
            [TurnItem::Plan(plan), TurnItem::AgentMessage(agent)]
                if plan.id == expected_plan_id
                    && !plan.text.trim().is_empty()
                    && agent.id == item_id =>
            {
                Some(plan)
            }
            _ => {
                return Err(
                    "final outbox requires its exact agent item, optionally preceded by one bound plan item, or exactly one bound plan item"
                        .to_string(),
                );
            }
        };
        if items.iter().any(|item| {
            matches!(item, TurnItem::AgentMessage(agent) if agent.id == item_id)
                && matches!(item, TurnItem::AgentMessage(agent) if agent.content.iter().all(|content| {
                    matches!(content, AgentMessageContent::Text { text } if text.trim().is_empty())
                }))
        }) {
            return Err(
                "final outbox agent item must contain visible text"
                    .to_string(),
            );
        }
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if document.active_turn_id.as_deref() != Some(turn_id) {
                    return Err(
                        "final outbox staging did not match the active task turn".to_string()
                    );
                }
                if document.final_emission_committed {
                    return Err("final outbox items cannot change after commit".to_string());
                }
                let task_generation = document.task_generation;
                let evidence_revision = document.accepted_evidence_revision;
                let Some(pending_index) = document.pending_finals.iter().position(|pending| {
                    pending.task_generation == task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                        && pending.persisted
                        && pending.emission_reserved
                        && !pending.superseded
                }) else {
                    return Err("final outbox staging had no persisted pending final".to_string());
                };
                if let Some(plan_item) = plan_item {
                    let expected_plan_text = document.pending_finals[pending_index]
                        .response_item
                        .as_ref()
                        .and_then(crate::stream_events_utils::raw_assistant_output_text_from_item)
                        .and_then(|text| extract_proposed_plan_text(&text))
                        .map(|text| strip_citations(&text).0);
                    if expected_plan_text.as_deref() != Some(plan_item.text.as_str()) {
                        return Err(
                            "final outbox plan item did not match the persisted provisional response"
                                .to_string(),
                        );
                    }
                }
                let pending = &mut document.pending_finals[pending_index];
                pending.emission_items = items.to_vec();
                pending.emission_key =
                    final_emission_key(task_generation, turn_id, item_id, evidence_revision, items);
                document.updated_at = timestamp();
                Ok(pending.emission_key.clone())
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let emission_key = result?;
        self.persist_required(&snapshot, "staging durable final outbox items")
            .await?;
        Ok(emission_key)
    }

    pub(crate) async fn recoverable_final_emission(
        &self,
    ) -> Result<Option<RecoverableFinalEmission>, String> {
        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return Ok(None);
        };
        let Some(committed) = document
            .committed_final
            .as_ref()
            .filter(|committed| !committed.completed)
        else {
            return Ok(None);
        };
        let pending = document
            .pending_finals
            .iter()
            .find(|pending| {
                pending.task_generation == committed.task_generation
                    && pending.turn_id == committed.turn_id
                    && pending.item_id == committed.item_id
                    && pending.persisted
                    && !pending.superseded
                    && pending.emission_key == committed.emission_key
                    && !pending.emission_key.is_empty()
                    && !pending.emission_items.is_empty()
            })
            .ok_or_else(|| {
                "durable final outbox commitment has no matching recoverable payload".to_string()
            })?;
        let terminal_event = committed.terminal_event.clone().ok_or_else(|| {
            "durable final outbox commitment has no recoverable terminal event".to_string()
        })?;
        Ok(Some(RecoverableFinalEmission {
            turn_id: committed.turn_id.clone(),
            item_id: committed.item_id.clone(),
            emission_key: committed.emission_key.clone(),
            items: pending.emission_items.clone(),
            terminal_event,
            terminal_event_staged: committed.terminal_event_staged,
        }))
    }

    pub(crate) async fn mark_final_item_completed(
        &self,
        turn_id: &str,
        item_id: &str,
    ) -> Result<(), String> {
        self.mark_final_emission_items_inner(turn_id, item_id, None)
            .await
    }

    pub(crate) async fn mark_final_emission_items_emitted(
        &self,
        turn_id: &str,
        item_id: &str,
        emission_key: &str,
    ) -> Result<(), String> {
        self.mark_final_emission_items_inner(turn_id, item_id, Some(emission_key))
            .await
    }

    async fn mark_final_emission_items_inner(
        &self,
        turn_id: &str,
        item_id: &str,
        emission_key: Option<&str>,
    ) -> Result<(), String> {
        let Some((matched, snapshot)) = self
            .update_document(|document| {
                let persisted = document.pending_finals.iter().any(|pending| {
                    pending.task_generation == document.task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                        && pending.persisted
                        && pending
                            .response_item
                            .as_ref()
                            .and_then(codex_protocol::models::ResponseItem::id)
                            == Some(item_id)
                        && emission_key
                            .is_none_or(|emission_key| pending.emission_key == emission_key)
                });
                let matched = persisted
                    && document.committed_final.as_ref().is_some_and(|committed| {
                        document.active_turn_id.as_deref() == Some(turn_id)
                            && committed.task_generation == document.task_generation
                            && committed.evidence_revision == document.accepted_evidence_revision
                            && committed.turn_id == turn_id
                            && committed.item_id == item_id
                            && !committed.completed
                            && emission_key
                                .is_none_or(|emission_key| committed.emission_key == emission_key)
                    });
                if matched {
                    if let Some(pending) = document.pending_finals.iter_mut().find(|pending| {
                        pending.task_generation == document.task_generation
                            && pending.turn_id == turn_id
                            && pending.item_id == item_id
                    }) {
                        pending.externally_emitted = true;
                        pending.externally_completed = true;
                    }
                    document.updated_at = timestamp();
                }
                matched
            })
            .await
        else {
            return Ok(());
        };
        if !matched {
            return Err("final item emission did not match the committed outbox".to_string());
        }
        self.persist_required(&snapshot, "recording durable final item emission")
            .await
    }

    pub(crate) async fn stage_final_terminal_event(
        &self,
        turn_id: &str,
        proposed_event: &EventMsg,
    ) -> Result<EventMsg, String> {
        if !matches!(
            proposed_event,
            EventMsg::TurnComplete(event) if event.turn_id == turn_id
        ) {
            return Err(
                "managed final terminal outbox requires a matching TurnComplete event".to_string(),
            );
        }
        let Some((result, snapshot)) = self
            .update_document(|document| {
                let task_generation = document.task_generation;
                let evidence_revision = document.accepted_evidence_revision;
                let item_id = document
                    .committed_final
                    .as_ref()
                    .filter(|committed| {
                        document.active_turn_id.as_deref() == Some(turn_id)
                            && committed.task_generation == task_generation
                            && committed.evidence_revision == evidence_revision
                            && committed.turn_id == turn_id
                            && !committed.completed
                    })
                    .map(|committed| committed.item_id.clone())
                    .ok_or_else(|| {
                        "terminal event did not match an incomplete committed final".to_string()
                    })?;
                let items_emitted = document.pending_finals.iter().any(|pending| {
                    pending.task_generation == task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                        && pending.externally_emitted
                        && pending.externally_completed
                        && !pending.superseded
                });
                if !items_emitted {
                    return Err(
                        "terminal event cannot be staged before final items are durable"
                            .to_string(),
                    );
                }
                let committed = document
                    .committed_final
                    .as_mut()
                    .expect("validated committed final");
                if committed.terminal_event_staged {
                    return committed.terminal_event.clone().ok_or_else(|| {
                        "staged terminal event is missing from the committed final".to_string()
                    });
                }
                let mut staged_event = proposed_event.clone();
                if let (EventMsg::TurnComplete(staged), Some(EventMsg::TurnComplete(fallback))) =
                    (&mut staged_event, committed.terminal_event.as_ref())
                    && staged.last_agent_message.is_none()
                {
                    staged.last_agent_message = fallback.last_agent_message.clone();
                }
                committed.terminal_event = Some(staged_event.clone());
                committed.terminal_event_staged = true;
                document.updated_at = timestamp();
                Ok(staged_event)
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let staged_event = result?;
        self.persist_required(&snapshot, "staging managed final terminal event")
            .await?;
        Ok(staged_event)
    }

    pub(crate) async fn mark_final_terminal_completed(
        &self,
        turn_id: &str,
        terminal_event: &EventMsg,
    ) -> Result<(), String> {
        let Some((matched, snapshot)) = self
            .update_document(|document| {
                let task_generation = document.task_generation;
                let evidence_revision = document.accepted_evidence_revision;
                let Some(item_id) = document
                    .committed_final
                    .as_ref()
                    .filter(|committed| {
                        document.active_turn_id.as_deref() == Some(turn_id)
                            && committed.task_generation == task_generation
                            && committed.evidence_revision == evidence_revision
                            && committed.turn_id == turn_id
                            && committed.terminal_event_staged
                            && committed.terminal_event.as_ref().is_some_and(|staged| {
                                serialized_values_equal(staged, terminal_event)
                            })
                    })
                    .map(|committed| committed.item_id.clone())
                else {
                    return false;
                };
                let items_emitted = document.pending_finals.iter().any(|pending| {
                    pending.task_generation == task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                        && pending.externally_emitted
                        && pending.externally_completed
                        && !pending.superseded
                });
                if !items_emitted {
                    return false;
                }
                if let Some(committed) = document.committed_final.as_mut() {
                    committed.completed = true;
                }
                document.updated_at = timestamp();
                true
            })
            .await
        else {
            return Ok(());
        };
        if !matched {
            return Err(
                "terminal completion did not match the staged managed final event".to_string(),
            );
        }
        self.persist_required(&snapshot, "completing managed final terminal outbox")
            .await
    }

    pub(crate) async fn abort_final_reservation(
        &self,
        turn_id: &str,
        item_id: &str,
    ) -> Result<(), String> {
        let Some((changed, snapshot)) = self
            .update_document(|document| {
                if document.active_turn_id.as_deref() != Some(turn_id) {
                    return false;
                }
                if document.committed_final.as_ref().is_some_and(|committed| {
                    committed.task_generation == document.task_generation
                        && committed.turn_id == turn_id
                        && committed.item_id == item_id
                }) {
                    return false;
                }
                let Some(pending) = document.pending_finals.iter_mut().find(|pending| {
                    pending.task_generation == document.task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                }) else {
                    return false;
                };
                if pending.externally_emitted {
                    return false;
                }
                supersede_pending_final(pending)
            })
            .await
        else {
            return Ok(());
        };
        if changed {
            self.persist_required(&snapshot, "aborting final output reservation")
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn abort_uncommitted_final_reservations_for_turn(
        &self,
        turn_id: &str,
    ) -> Result<(), String> {
        let Some((changed, snapshot)) = self
            .update_document(|document| {
                if document.active_turn_id.as_deref() != Some(turn_id)
                    || document.committed_final.as_ref().is_some_and(|committed| {
                        committed.task_generation == document.task_generation
                            && committed.turn_id == turn_id
                    })
                {
                    return false;
                }
                let mut changed = false;
                for pending in &mut document.pending_finals {
                    if pending.task_generation == document.task_generation
                        && pending.turn_id == turn_id
                        && !pending.externally_emitted
                        && !pending.superseded
                    {
                        changed |= supersede_pending_final(pending);
                    }
                }
                changed
            })
            .await
        else {
            return Ok(());
        };
        if changed {
            self.persist_required(&snapshot, "aborting uncommitted final outputs for turn")
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn mark_final_item_persisted(
        &self,
        turn_id: &str,
        item_id: &str,
        item: &codex_protocol::models::ResponseItem,
    ) -> Result<(), String> {
        let Some((matched, snapshot)) = self
            .update_document(|document| {
                if document.active_turn_id.as_deref() != Some(turn_id) {
                    return false;
                }
                if let Some(pending) = document.pending_finals.iter_mut().find(|pending| {
                    pending.task_generation == document.task_generation
                        && pending.turn_id == turn_id
                        && pending.item_id == item_id
                }) {
                    pending.persisted = true;
                    pending.response_item = Some(item.clone());
                    true
                } else {
                    false
                }
            })
            .await
        else {
            return Ok(());
        };
        if !matched {
            return Err("provisional final had no matching pending record".to_string());
        }
        self.persist_required(&snapshot, "persisting provisional final state")
            .await
    }

    async fn snapshot_known_roots_and_task_changes(
        &self,
    ) -> (
        u64,
        BTreeMap<String, RepositoryState>,
        BTreeMap<String, BTreeSet<String>>,
        BTreeMap<String, ExplicitMutationTargetState>,
    ) {
        let (document_revision, roots_baselines_and_targets, non_git_roots) = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| {
                    (
                        document.revision,
                        document
                            .known_roots
                            .keys()
                            .map(|root| {
                                (
                                    root.clone(),
                                    document.root_baselines.get(root).cloned(),
                                    document
                                        .mutation_targets
                                        .iter()
                                        .filter_map(|(target, owner)| {
                                            (owner == root).then_some(target.clone())
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            })
                            .collect::<Vec<_>>(),
                        document
                            .non_git_root_snapshots
                            .keys()
                            .cloned()
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .unwrap_or_default()
        };
        let mut states = BTreeMap::new();
        let mut task_changed_paths = BTreeMap::new();
        for (registered_root, baseline, targets) in roots_baselines_and_targets {
            let mut state = snapshot_repository_state(Path::new(&registered_root)).await;
            for target in targets {
                let target_state = snapshot_explicit_mutation_target(Path::new(&target)).await;
                if let Some(read_error) = target_state.read_error.as_ref() {
                    state.available = false;
                    let target_error = format!(
                        "could not freeze explicit mutation target `{target}`: {read_error}"
                    );
                    state.error = Some(match state.error.take() {
                        Some(existing) => format!("{existing}; {target_error}"),
                        None => target_error,
                    });
                }
                state
                    .explicit_target_snapshots
                    .insert(target.clone(), target_state);
            }
            let changed =
                task_changed_paths_from_baseline(Path::new(&state.root), baseline.as_ref(), &state)
                    .await;
            if !changed.is_empty() {
                task_changed_paths.insert(state.root.clone(), changed);
            }
            states.insert(state.root.clone(), state);
        }
        let non_git_root_snapshots = snapshot_non_git_roots(&non_git_roots).await;
        (
            document_revision,
            states,
            task_changed_paths,
            non_git_root_snapshots,
        )
    }

    pub(crate) async fn begin_verify_local_validation(
        &self,
    ) -> Option<TaskEvidenceValidationStart> {
        let repo_root = self.repo_root.as_ref()?;
        let (epoch, mut file_paths, artifact_paths) = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            let mut file_paths = document
                .latest_file_hashes
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            for step in &document.plan {
                file_paths.extend(step.edit_paths.iter().cloned());
            }
            let artifact_paths = document
                .generated_artifact_requirements
                .iter()
                .filter_map(|requirement| requirement.path.clone())
                .collect::<BTreeSet<_>>();
            (document.evidence_epoch, file_paths, artifact_paths)
        };
        file_paths.extend(git_dirty_paths(repo_root).await);
        let mut file_snapshots = BTreeMap::new();
        for path in file_paths {
            file_snapshots.insert(path.clone(), snapshot_file(repo_root, &path).await);
        }
        let mut artifact_snapshots = BTreeMap::new();
        for path in artifact_paths {
            artifact_snapshots.insert(path.clone(), snapshot_file(repo_root, &path).await);
        }
        Some(TaskEvidenceValidationStart {
            epoch,
            file_snapshots,
            artifact_snapshots,
        })
    }

    pub(crate) async fn try_record_plan_update(
        &self,
        update: &UpdatePlanArgs,
    ) -> Result<UpdatePlanArgs, String> {
        let Some(_) = self.repo_root else {
            return Ok(update.clone());
        };
        let Some(((response, previous_document), snapshot)) = self
            .update_document(|document| {
                let previous_document = document.clone();
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
                    let status = normalize_requested_status(
                        &item.status,
                        old.map(|step| &step.status),
                        material_step_change,
                    );
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
                        validation_receipt_ids: old
                            .filter(|_| !material_step_change)
                            .map_or_else(Vec::new, |step| step.validation_receipt_ids.clone()),
                    });
                }
                material_plan_change |= previous
                    .keys()
                    .any(|id| !normalized.iter().any(|step| &step.id == id));
                document.plan = normalized;
                if material_plan_change {
                    invalidate_for_scope_change(document);
                }
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                rebuild_declared_requirements_and_risks(document);
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                promote_steps_with_fresh_evidence(document);
                document.updated_at = timestamp();
                document.completion = None;
                (
                    UpdatePlanArgs {
                        explanation: update.explanation.clone(),
                        plan: document.plan.iter().map(plan_item_from_evidence).collect(),
                    },
                    previous_document,
                )
            })
            .await
        else {
            return Ok(update.clone());
        };
        if self.persist_document(&snapshot).await == PersistOutcome::Failed {
            let fail_closed_snapshot = {
                let mut guard = self.document.lock().await;
                let Some(document) = guard.as_mut() else {
                    return Err("plan update could not be persisted durably".to_string());
                };
                if document.revision == snapshot.revision {
                    *document = previous_document;
                    None
                } else {
                    fail_closed_for_storage(
                        document,
                        "plan invalidation could not be persisted durably",
                    );
                    document.revision = document.revision.saturating_add(1);
                    Some(document.clone())
                }
            };
            if let Some(fail_closed_snapshot) = fail_closed_snapshot {
                let _ = self.persist_document(&fail_closed_snapshot).await;
            }
            return Err("plan update could not be persisted durably".to_string());
        }
        Ok(response)
    }

    #[cfg(test)]
    pub(crate) async fn record_plan_update(&self, update: &UpdatePlanArgs) -> UpdatePlanArgs {
        self.try_record_plan_update(update)
            .await
            .unwrap_or_else(|_| update.clone())
    }

    pub(crate) async fn record_edit_intent(&self, call_id: &str, cwd: &Path, paths: &[PathBuf]) {
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

        let Some((_, snapshot)) = self
            .update_document(|document| {
                document
                    .edit_intents
                    .retain(|intent| intent.call_id != call_id);
                document.edit_intents.push(EditIntent {
                    call_id: call_id.to_string(),
                    step_id: document.active_step_id.clone(),
                    started_at: timestamp(),
                    completed_at: None,
                    outcome: None,
                    files,
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

    pub(crate) async fn record_edit_result(&self, call_id: &str, outcome: &str) {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let intent = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .and_then(|document| {
                    document
                        .edit_intents
                        .iter()
                        .find(|intent| intent.call_id == call_id)
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
        let (snapshot_revision, root_states, task_changed_paths, non_git_root_snapshots) =
            if transitions.is_empty() {
                (0, BTreeMap::new(), BTreeMap::new(), BTreeMap::new())
            } else {
                self.snapshot_known_roots_and_task_changes().await
            };

        let Some((_, snapshot)) = self
            .update_document(|document| {
                if let Some(stored) = document
                    .edit_intents
                    .iter_mut()
                    .find(|stored| stored.call_id == call_id)
                {
                    stored.completed_at = Some(timestamp());
                    stored.outcome = Some(outcome.to_string());
                }
                if !transitions.is_empty() {
                    let snapshot_is_current = document.revision == snapshot_revision;
                    if document.classification.is_none() {
                        invalidate_for_mutation(document);
                    }
                    if snapshot_is_current {
                        apply_root_snapshots(document, root_states, task_changed_paths);
                        document.non_git_root_snapshots = non_git_root_snapshots;
                    }
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
                            step.validation_receipt_ids.clear();
                        }
                    }
                    if affected_steps.is_empty() {
                        upsert_risk(
                            document,
                            EvidenceRisk {
                                id: format!("unassociated-edit-{call_id}"),
                                description: format!(
                                    "edit `{call_id}` changed files without an active plan step"
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
                        call_id: call_id.to_string(),
                        step_id: intent.step_id,
                        recorded_at: timestamp(),
                        epoch,
                        outcome: outcome.to_string(),
                        files: transitions,
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
        if self.repo_root.is_none() {
            return;
        }
        let (target_discovery_complete, targets_accounted) = if possible_mutation {
            match cwd.to_abs_path() {
                Ok(native_cwd) => {
                    let targets = command_mutation_targets(command, native_cwd.as_path());
                    let accounted = self
                        .reconcile_command_mutation_targets(native_cwd.as_path(), &targets.paths)
                        .await
                        .is_ok();
                    (targets.discovery_complete, accounted)
                }
                Err(_) => (false, false),
            }
        } else {
            (true, true)
        };
        let (snapshot_revision, root_states, task_changed_paths, non_git_root_snapshots) =
            if possible_mutation {
                self.snapshot_known_roots_and_task_changes().await
            } else {
                (0, BTreeMap::new(), BTreeMap::new(), BTreeMap::new())
            };
        let mutation_snapshot_is_bounded = target_discovery_complete
            && targets_accounted
            && !root_states.is_empty()
            && root_states.values().all(|state| state.available);
        let command_succeeded = exit_code == 0 && !timed_out;
        let Some((_, snapshot)) = self
            .update_document(|document| {
                if possible_mutation {
                    let snapshot_is_current = document.revision == snapshot_revision;
                    if document.classification.is_none() {
                        invalidate_for_mutation(document);
                    }
                    if snapshot_is_current {
                        apply_root_snapshots(document, root_states, task_changed_paths);
                        document.non_git_root_snapshots = non_git_root_snapshots;
                    }
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
                            resolved: mutation_snapshot_is_bounded && snapshot_is_current,
                            epoch,
                        },
                    );
                }
                let receipt_id = next_receipt_id(
                    "command",
                    &mut document.next_command_receipt_sequence,
                );
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_verify_local(
        &self,
        mode: &str,
        verdict: Option<&str>,
        tool_success: bool,
        proof_bearing: bool,
        validation_start: Option<&TaskEvidenceValidationStart>,
        active_files: &[PathBuf],
        stale_reasons: &[String],
        payload: Option<&Value>,
    ) -> bool {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        let normalized_active_files = active_files
            .iter()
            .map(|path| normalize_input_path(repo_root, Some(repo_root), path))
            .collect::<Vec<_>>();
        let mut file_snapshots = Vec::with_capacity(normalized_active_files.len());
        for path in &normalized_active_files {
            file_snapshots.push(snapshot_file(repo_root, path).await);
        }
        file_snapshots.sort_by(|left, right| left.path.cmp(&right.path));
        file_snapshots.dedup_by(|left, right| left.path == right.path);
        let mut canonical_stale_reasons = stale_reasons.to_vec();
        canonical_stale_reasons.sort();
        canonical_stale_reasons.dedup();
        let mut validation_end_files = BTreeMap::new();
        let mut validation_end_artifacts = BTreeMap::new();
        if let Some(start) = validation_start {
            for path in start.file_snapshots.keys() {
                validation_end_files.insert(path.clone(), snapshot_file(repo_root, path).await);
            }
            for path in start.artifact_snapshots.keys() {
                validation_end_artifacts.insert(path.clone(), snapshot_file(repo_root, path).await);
            }
        }
        let snapshots_unchanged = validation_start.is_some_and(|start| {
            normalized_active_files
                .iter()
                .all(|path| start.file_snapshots.contains_key(path))
                && start.file_snapshots == validation_end_files
                && start.artifact_snapshots == validation_end_artifacts
        });

        let Some((accepted_proof, snapshot)) = self
            .update_document(|document| {
                let run_matches_start = validation_start.is_some_and(|start| {
                    start.epoch == document.evidence_epoch && snapshots_unchanged
                });
                let accepted_proof = proof_bearing
                    && tool_success
                    && run_matches_start
                    && verdict == Some("VERIFIED")
                    && canonical_stale_reasons.is_empty()
                    && file_snapshots
                        .iter()
                        .all(|snapshot| snapshot.read_error.is_none());
                let invalidates_prior_proof =
                    matches!(mode, "fast" | "final") && !accepted_proof;
                if invalidates_prior_proof {
                    invalidate_for_failed_validation(document);
                }
                let receipt_id =
                    next_receipt_id("validation", &mut document.next_validation_receipt_sequence);
                document.validation_receipts.push(ValidationReceipt {
                    id: receipt_id.clone(),
                    recorded_at: timestamp(),
                    epoch: document.evidence_epoch,
                    step_id: document.active_step_id.clone(),
                    mode: mode.to_string(),
                    verdict: verdict.map(str::to_string),
                    tool_success,
                    proof_bearing,
                    accepted_proof,
                    active_files: file_snapshots.clone(),
                    stale_reasons: canonical_stale_reasons.clone(),
                    payload: payload.cloned(),
                });
                trim_to_last(&mut document.validation_receipts, MAX_VALIDATION_RECEIPTS);

                if mode == "plan" && tool_success && run_matches_start {
                    document.verify_plan_epoch = Some(document.evidence_epoch);
                    rebuild_verifier_requirements(document, payload);
                }
                if accepted_proof {
                    document.validation_epoch = Some(document.evidence_epoch);
                    for snapshot in validation_end_files.values() {
                        document
                            .latest_file_hashes
                            .insert(snapshot.path.clone(), snapshot.clone());
                    }
                    for snapshot in validation_end_artifacts.values() {
                        document
                            .generated_artifact_hashes
                            .insert(snapshot.path.clone(), snapshot.clone());
                        document
                            .latest_generated_artifact_hashes
                            .insert(snapshot.path.clone(), snapshot.clone());
                    }
                    for step in &mut document.plan {
                        if !step.edit_paths.is_empty()
                            && step.edit_paths.iter().all(|path| {
                                file_snapshots.iter().any(|active| {
                                    active.read_error.is_none()
                                        && path_is_covered(path, &active.path)
                                })
                            })
                        {
                            step.validation_receipt_ids.push(receipt_id.clone());
                            step.validation_receipt_ids.sort();
                            step.validation_receipt_ids.dedup();
                        }
                    }
                    for requirement in &mut document.generated_artifact_requirements {
                        if requirement.path.is_none()
                            && verifier_requirement_satisfied(requirement, payload)
                        {
                            requirement.validation_receipt_ids.push(receipt_id.clone());
                            requirement.validation_receipt_ids.sort();
                            requirement.validation_receipt_ids.dedup();
                        }
                    }
                    resolve_risks_by_source(document, "verify_local");
                    resolve_risks_by_source(document, "generated_artifact_freshness");
                    resolve_risks_by_source(document, "freshness");
                } else if invalidates_prior_proof {
                    if proof_bearing && tool_success && !run_matches_start {
                        upsert_risk(
                            document,
                            EvidenceRisk {
                                id: "verify-local-concurrent-change".to_string(),
                                description: "task-controlled files, generated artifacts, or the evidence epoch changed while verify_local was running"
                                    .to_string(),
                                source: "verify_local".to_string(),
                                blocking: false,
                                resolved: false,
                                epoch: document.evidence_epoch,
                            },
                        );
                    }
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("verify-local-{mode}-failed"),
                            description: format!(
                                "the latest {mode} verification attempt was not accepted as fresh proof"
                            ),
                            source: "verify_local".to_string(),
                            blocking: true,
                            resolved: false,
                            epoch: document.evidence_epoch,
                        },
                    );
                } else if proof_bearing && tool_success && !run_matches_start {
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: "verify-local-concurrent-change".to_string(),
                            description: "task-controlled files, generated artifacts, or the evidence epoch changed while verify_local was running"
                                .to_string(),
                            source: "verify_local".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch: document.evidence_epoch,
                        },
                    );
                } else if verdict == Some("NEEDS_REGEN") {
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: "verify-local-needs-regen".to_string(),
                            description:
                                "verify_local reported required generated artifacts are stale"
                                    .to_string(),
                            source: "verify_local".to_string(),
                            blocking: true,
                            resolved: false,
                            epoch: document.evidence_epoch,
                        },
                    );
                } else if !stale_reasons.is_empty() {
                    for (index, reason) in stale_reasons.iter().enumerate() {
                        upsert_risk(
                            document,
                            EvidenceRisk {
                                id: format!("verify-local-stale-{index}"),
                                description: reason.clone(),
                                source: "verify_local".to_string(),
                                blocking: false,
                                resolved: false,
                                epoch: document.evidence_epoch,
                            },
                        );
                    }
                }
                promote_steps_with_fresh_evidence(document);
                document.updated_at = timestamp();
                document.completion = None;
                accepted_proof
            })
            .await
        else {
            return false;
        };
        self.persist_document(&snapshot).await;
        accepted_proof
    }

    pub(crate) async fn take_finalization_warning(&self) -> Option<String> {
        if self
            .document
            .lock()
            .await
            .as_ref()
            .is_some_and(|document| document.classification.is_some())
        {
            return None;
        }
        let gate = self.completion_gate().await?;
        if gate.status == TaskCompletionStatus::Passed {
            return None;
        }
        let (should_warn, snapshot) = self
            .update_document(|document| {
                if document.repair_turns_used >= 1 {
                    return None;
                }
                document.repair_turns_used += 1;
                document.updated_at = timestamp();
                Some(())
            })
            .await?;
        should_warn?;
        self.persist_document(&snapshot).await;

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
            "KD4 task evidence is {status}: {reason_summary}{remaining}. No automatic repair turn was started.",
            status = completion_status_name(gate.status),
        ))
    }

    pub(crate) async fn take_automatic_verify_plan_request(&self) -> Option<Vec<String>> {
        if self
            .repo_root
            .as_ref()
            .is_none_or(|root| !root.join("scripts").join("verify_local.py").is_file())
        {
            return None;
        }
        let (changed_paths, snapshot) = self
            .update_document(|document| {
                let has_mutation = !document.edit_receipts.is_empty()
                    || document
                        .command_receipts
                        .iter()
                        .any(|receipt| receipt.possible_mutation);
                if !has_mutation
                    || document.verify_plan_epoch == Some(document.evidence_epoch)
                    || document.automatic_plan_attempt_epoch == Some(document.evidence_epoch)
                {
                    return None;
                }
                document.automatic_plan_attempt_epoch = Some(document.evidence_epoch);
                document.updated_at = timestamp();
                Some(document.latest_file_hashes.keys().cloned().collect())
            })
            .await?;
        let changed_paths = changed_paths?;
        self.persist_document(&snapshot).await;
        Some(changed_paths)
    }

    pub(crate) async fn completion_gate(&self) -> Option<TaskCompletionGate> {
        let lifecycle_managed = self
            .document
            .lock()
            .await
            .as_ref()
            .is_some_and(|document| document.classification.is_some());
        if lifecycle_managed {
            let (gate, snapshot) = self
                .update_document(|document| {
                    let outcome = document
                        .outcome
                        .filter(|_| document.phase == TaskPhase::Ready)?;
                    let Some(turn_id) = document.active_turn_id.as_deref() else {
                        return None;
                    };
                    let final_items_emitted = exact_final_items_emitted(document, turn_id);
                    if !final_items_emitted {
                        return None;
                    }
                    let status = match outcome {
                        TaskOutcome::Passed => TaskCompletionStatus::Passed,
                        TaskOutcome::Partial => TaskCompletionStatus::Partial,
                        TaskOutcome::Blocked => TaskCompletionStatus::Blocked,
                    };
                    let gate = TaskCompletionGate {
                        status,
                        reasons: if outcome == TaskOutcome::Passed {
                            Vec::new()
                        } else {
                            vec![
                                "runtime correctness-closure terminated with a non-passing outcome"
                                    .to_string(),
                            ]
                        },
                        evidence_path: self
                            .evidence_path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned()),
                    };
                    document.completion = Some(gate.clone());
                    Some(gate)
                })
                .await?;
            let gate = gate?;
            return match self.persist_document(&snapshot).await {
                PersistOutcome::Persisted => Some(gate),
                PersistOutcome::Superseded | PersistOutcome::Failed => Some(
                    self.demote_gate_for_persistence(
                        gate,
                        Some(snapshot.revision),
                        "task-evidence persistence failed at lifecycle finalization",
                    )
                    .await,
                ),
            };
        }
        let mut latest_gate = None;
        for _ in 0..8 {
            let _ = self.refresh_external_file_freshness().await;
            let (gate, snapshot) = self
                .update_document(|document| {
                    if !task_is_tracked(document) {
                        return None;
                    }
                    promote_steps_with_fresh_evidence(document);
                    let gate = derive_completion_gate(document, self.evidence_path.as_deref());
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
                        self.demote_gate_for_persistence(
                            gate,
                            Some(snapshot.revision),
                            "task-evidence persistence failed; completion is not durably recorded",
                        )
                        .await,
                    );
                }
            }
        }
        let gate = latest_gate?;
        Some(
            self.demote_gate_for_persistence(
                gate,
                None,
                "task-evidence changed repeatedly while completion was being persisted; a stable completion snapshot was not recorded",
            )
            .await,
        )
    }

    async fn demote_gate_for_persistence(
        &self,
        mut gate: TaskCompletionGate,
        snapshot_revision: Option<u64>,
        reason: &str,
    ) -> TaskCompletionGate {
        gate.reasons.push(reason.to_string());
        gate.reasons.sort();
        gate.reasons.dedup();
        if gate.status == TaskCompletionStatus::Passed {
            gate.status = TaskCompletionStatus::Partial;
        }
        let mut guard = self.document.lock().await;
        if let Some(document) = guard.as_mut()
            && snapshot_revision == Some(document.revision)
        {
            document.completion = Some(gate.clone());
        }
        gate
    }

    #[allow(dead_code)]
    pub(crate) async fn record_desktop_activation(
        &self,
        process_path: String,
        binary_sha1: String,
        runtime_evidence: String,
    ) {
        let Some((_, snapshot)) = self
            .update_document(|document| {
                document.desktop_activation_receipt = Some(DesktopActivationReceipt {
                    epoch: document.evidence_epoch,
                    recorded_at: timestamp(),
                    process_path,
                    binary_sha1,
                    runtime_evidence,
                });
                promote_steps_with_fresh_evidence(document);
                document.updated_at = timestamp();
                document.completion = None;
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    async fn refresh_external_file_freshness(&self) -> Result<bool, String> {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return Ok(false);
        };
        loop {
            let (expected_revision, expected, expected_artifacts) = {
                let guard = self.document.lock().await;
                guard
                    .as_ref()
                    .map(|document| {
                        (
                            document.revision,
                            document.latest_file_hashes.clone(),
                            document.generated_artifact_hashes.clone(),
                        )
                    })
                    .unwrap_or_default()
            };
            if expected.is_empty() && expected_artifacts.is_empty() {
                return Ok(false);
            }
            let mut changed = Vec::new();
            for (path, previous) in expected {
                let current = snapshot_file(repo_root, &path).await;
                if current != previous {
                    changed.push((previous, current));
                }
            }
            let mut changed_artifacts = Vec::new();
            for (path, previous) in expected_artifacts {
                let current = snapshot_file(repo_root, &path).await;
                if current != previous {
                    changed_artifacts.push((previous, current));
                }
            }
            if changed.is_empty() && changed_artifacts.is_empty() {
                let guard = self.document.lock().await;
                if guard
                    .as_ref()
                    .is_some_and(|document| document.revision != expected_revision)
                {
                    continue;
                }
                return Ok(false);
            }
            let (
                snapshot_revision,
                current_roots,
                current_task_changed_paths,
                current_non_git_roots,
            ) = self.snapshot_known_roots_and_task_changes().await;
            if snapshot_revision != expected_revision {
                continue;
            }

            let update = self
            .update_document_if_revision(snapshot_revision, |document| {
                let changed = changed
                    .into_iter()
                    .filter(|(previous, current)| {
                        document.latest_file_hashes.get(&current.path) == Some(previous)
                    })
                    .map(|(_, current)| current)
                    .collect::<Vec<_>>();
                let changed_artifacts = changed_artifacts
                    .into_iter()
                    .filter(|(previous, current)| {
                        document.generated_artifact_hashes.get(&current.path) == Some(previous)
                    })
                    .map(|(_, current)| current)
                    .collect::<Vec<_>>();
                if changed.is_empty() && changed_artifacts.is_empty() {
                    return false;
                }
                invalidate_for_mutation(document);
                apply_root_snapshots(document, current_roots, current_task_changed_paths);
                document.non_git_root_snapshots = current_non_git_roots;
                let epoch = document.evidence_epoch;
                for current in changed {
                    let path = current.path.clone();
                    if current.read_error.is_some() {
                        upsert_risk(document, unreadable_file_risk(&path, epoch, "freshness"));
                    } else {
                        resolve_risk(document, &unreadable_file_risk_id(&path));
                    }
                    document
                        .latest_file_hashes
                        .insert(path.clone(), current);
                    for step in &mut document.plan {
                        if step.edit_paths.contains(&path)
                            && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                        {
                            step.status = StepStatus::Implemented;
                            step.validation_receipt_ids.clear();
                        }
                    }
                }
                for current in changed_artifacts {
                    let path = current.path.clone();
                    document.generated_artifact_hashes.remove(&path);
                    document
                        .latest_generated_artifact_hashes
                        .insert(path.clone(), current);
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: generated_artifact_freshness_risk_id(&path),
                            description: format!(
                                "generated artifact `{path}` changed or became unreadable after validation"
                            ),
                            source: "generated_artifact_freshness".to_string(),
                            blocking: true,
                            resolved: false,
                            epoch,
                        },
                    );
                }
                upsert_risk(
                    document,
                    EvidenceRisk {
                        id: format!("external-change-{epoch}"),
                        description: "a task-controlled file changed after its recorded evidence"
                            .to_string(),
                        source: "freshness".to_string(),
                        blocking: false,
                        resolved: false,
                        epoch,
                    },
                );
                document.updated_at = timestamp();
                true
            })
            .await;
            let (changed, snapshot) = match update {
                None => return Ok(false),
                Some(Err(())) => continue,
                Some(Ok(update)) => update,
            };
            if changed {
                self.persist_required(&snapshot, "persisting external freshness invalidation")
                    .await?;
            }
            return Ok(changed);
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

    async fn update_document_if_revision<T>(
        &self,
        expected_revision: u64,
        update: impl FnOnce(&mut TaskEvidenceDocument) -> T,
    ) -> Option<Result<(T, TaskEvidenceDocument), ()>> {
        let mut guard = self.document.lock().await;
        let document = guard.as_mut()?;
        if document.revision != expected_revision {
            return Some(Err(()));
        }
        let result = update(document);
        document.revision = document.revision.saturating_add(1);
        Some(Ok((result, document.clone())))
    }

    async fn persist_required(
        &self,
        document: &TaskEvidenceDocument,
        operation: &str,
    ) -> Result<(), String> {
        match self.persist_document(document).await {
            PersistOutcome::Persisted | PersistOutcome::Superseded => Ok(()),
            PersistOutcome::Failed => {
                let mut guard = self.document.lock().await;
                if let Some(current) = guard.as_mut()
                    && current.revision == document.revision
                {
                    fail_closed_for_storage(
                        current,
                        &format!("{operation} was not durably persisted"),
                    );
                    current.revision = current.revision.saturating_add(1);
                }
                Err(format!(
                    "{operation} failed because task evidence could not be persisted"
                ))
            }
        }
    }

    async fn persist_document(&self, document: &TaskEvidenceDocument) -> PersistOutcome {
        if self.persistence == TaskEvidencePersistence::Disabled {
            return PersistOutcome::Failed;
        }
        let _persistence_permit = match self.persistence_gate.acquire().await {
            Ok(permit) => permit,
            Err(err) => {
                warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                return PersistOutcome::Failed;
            }
        };
        let last_persisted_revision = self.last_persisted_revision.load(Ordering::Acquire);
        if last_persisted_revision != 0 {
            if last_persisted_revision > document.revision {
                return PersistOutcome::Superseded;
            }
            if last_persisted_revision == document.revision {
                return PersistOutcome::Persisted;
            }
        }
        if self.persistence == TaskEvidencePersistence::InMemory {
            self.last_persisted_revision
                .store(document.revision, Ordering::Release);
            return PersistOutcome::Persisted;
        }
        let Some(path) = self.evidence_path.as_ref() else {
            warn!("KD4 file-backed task evidence is missing its persistence path");
            return PersistOutcome::Failed;
        };
        let bytes = match serde_json::to_vec_pretty(document) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("failed to serialize KD4 task evidence: {err}");
                return PersistOutcome::Failed;
            }
        };
        let write_path = path.clone();
        match tokio::task::spawn_blocking(move || atomic_write_evidence(&write_path, &bytes)).await
        {
            Ok(Ok(())) => {
                self.last_persisted_revision
                    .store(document.revision, Ordering::Release);
                PersistOutcome::Persisted
            }
            Ok(Err(err)) => {
                warn!("failed to persist KD4 task evidence: {err}");
                PersistOutcome::Failed
            }
            Err(err) => {
                warn!("KD4 task-evidence persistence task failed: {err}");
                PersistOutcome::Failed
            }
        }
    }
}

async fn new_task_evidence_document(
    thread_id: String,
    cwd: &Path,
    repo_root: &Path,
    repo_is_git: bool,
    storage_failure_reason: Option<&str>,
    now: String,
) -> TaskEvidenceDocument {
    let git = collect_git_info(repo_root).await;
    let repository_root = repo_root.to_string_lossy().into_owned();
    let mut known_roots = BTreeMap::new();
    if repo_is_git {
        let initial_root_state = snapshot_repository_state(repo_root).await;
        known_roots.insert(repository_root.clone(), initial_root_state);
    }
    let root_baselines = known_roots.clone();
    TaskEvidenceDocument {
        schema_version: TASK_EVIDENCE_SCHEMA_VERSION,
        revision: 1,
        thread_id,
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
        validation_receipts: Vec::new(),
        generated_artifact_requirements: Vec::new(),
        generated_artifact_hashes: BTreeMap::new(),
        latest_generated_artifact_hashes: BTreeMap::new(),
        latest_file_hashes: BTreeMap::new(),
        risks: storage_failure_reason
            .map(|reason| vec![task_evidence_storage_risk(reason, 0)])
            .unwrap_or_default(),
        verify_plan_epoch: None,
        validation_epoch: None,
        desktop_activation_receipt: None,
        automatic_plan_attempt_epoch: None,
        repair_turns_used: 0,
        next_edit_receipt_sequence: initial_receipt_sequence(),
        next_command_receipt_sequence: initial_receipt_sequence(),
        next_validation_receipt_sequence: initial_receipt_sequence(),
        completion: None,
        active_turn_id: None,
        task_contract: String::new(),
        task_generation: 0,
        phase: TaskPhase::Unclassified,
        outcome: None,
        mutation_revision: 0,
        accepted_evidence_revision: 0,
        classification: None,
        investigation_checkpoint_hash: None,
        known_roots,
        root_baselines,
        task_changed_paths: BTreeMap::new(),
        observed_roots: BTreeSet::new(),
        supported_non_git_roots: BTreeSet::new(),
        non_git_root_snapshots: BTreeMap::new(),
        unsupported_mutation_targets: BTreeSet::new(),
        pass_prohibited_mutation_targets: BTreeSet::new(),
        mutation_targets: BTreeMap::new(),
        descendant_evidence_hashes: BTreeMap::new(),
        accepted_receipt_hashes: BTreeSet::new(),
        accepted_closure: None,
        review_findings: BTreeSet::new(),
        latest_review_finding_revision: None,
        actionable_findings: BTreeSet::new(),
        latest_actionable_finding_revision: None,
        clean_review_hash: None,
        prepared_review: None,
        review_attempt_failures: BTreeMap::new(),
        closure_fingerprint: None,
        incomplete_occurrences: BTreeMap::new(),
        pending_finals: Vec::new(),
        final_emission_committed: false,
        committed_final: None,
    }
}

fn rebase_final_fence_for_repository_change(
    mut fresh: TaskEvidenceDocument,
    mut previous: TaskEvidenceDocument,
) -> TaskEvidenceDocument {
    fresh.revision = previous.revision.saturating_add(1);
    let final_emission_committed = previous.final_emission_committed;
    let incomplete_committed = previous
        .committed_final
        .take()
        .filter(|committed| final_emission_committed && !committed.completed);
    for pending in &mut previous.pending_finals {
        let belongs_to_committed = incomplete_committed.as_ref().is_some_and(|committed| {
            pending.task_generation == committed.task_generation
                && pending.turn_id == committed.turn_id
                && pending.item_id == committed.item_id
        });
        if !belongs_to_committed {
            supersede_pending_final(pending);
        }
    }
    fresh.pending_finals = previous.pending_finals;
    if let Some(committed) = incomplete_committed {
        fresh.task_generation = committed.task_generation;
        fresh.accepted_evidence_revision = committed.evidence_revision;
        fresh.active_turn_id = Some(committed.turn_id.clone());
        fresh.final_emission_committed = true;
        fresh.committed_final = Some(committed);
    } else {
        fresh.task_generation = previous.task_generation.saturating_add(1);
    }
    repair_task_lifecycle_invariants(&mut fresh);
    fresh
}

enum ExistingDocument {
    Missing,
    Loaded {
        document: Box<TaskEvidenceDocument>,
        repository_changed: bool,
    },
    Rejected {
        kind: &'static str,
        reason: String,
    },
}

async fn load_existing_document(
    path: &Path,
    expected_thread_id: &str,
    expected_repository_root: &str,
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
    let schema_version = match value
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
    {
        Some(schema_version) => schema_version,
        None => {
            return ExistingDocument::Rejected {
                kind: "incompatible",
                reason: "missing numeric schema_version".to_string(),
            };
        }
    };
    if !(1..=TASK_EVIDENCE_SCHEMA_VERSION).contains(&schema_version) {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: format!("unsupported schema version {schema_version}"),
        };
    }
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
    let repository_changed = document.start.repository_root != expected_repository_root;
    ExistingDocument::Loaded {
        document: Box::new(document),
        repository_changed,
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

fn reset_task_document(document: &mut TaskEvidenceDocument, root_state: Option<RepositoryState>) {
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.last_mutation_at = None;
    document.plan.clear();
    document.active_step_id = None;
    document.edit_intents.clear();
    document.edit_receipts.clear();
    document.command_receipts.clear();
    document.validation_receipts.clear();
    document.generated_artifact_requirements.clear();
    document.generated_artifact_hashes.clear();
    document.latest_generated_artifact_hashes.clear();
    document.latest_file_hashes.clear();
    document.risks.clear();
    document.verify_plan_epoch = None;
    document.validation_epoch = None;
    document.desktop_activation_receipt = None;
    document.automatic_plan_attempt_epoch = None;
    document.repair_turns_used = 0;
    document.completion = None;
    document.active_turn_id = None;
    document.task_contract.clear();
    document.phase = TaskPhase::Unclassified;
    document.outcome = None;
    document.mutation_revision = 0;
    document.accepted_evidence_revision = 0;
    document.classification = None;
    document.investigation_checkpoint_hash = None;
    document.known_roots.clear();
    document.root_baselines.clear();
    document.task_changed_paths.clear();
    if let Some(root_state) = root_state {
        document
            .root_baselines
            .insert(root_state.root.clone(), root_state.clone());
        document
            .known_roots
            .insert(root_state.root.clone(), root_state);
    }
    document.observed_roots.clear();
    document.supported_non_git_roots.clear();
    document.non_git_root_snapshots.clear();
    document.unsupported_mutation_targets.clear();
    document.pass_prohibited_mutation_targets.clear();
    document.mutation_targets.clear();
    document.descendant_evidence_hashes.clear();
    document.accepted_receipt_hashes.clear();
    document.accepted_closure = None;
    document.review_findings.clear();
    document.latest_review_finding_revision = None;
    document.actionable_findings.clear();
    document.latest_actionable_finding_revision = None;
    document.clean_review_hash = None;
    document.prepared_review = None;
    document.review_attempt_failures.clear();
    document.closure_fingerprint = None;
    document.incomplete_occurrences.clear();
    for pending in &mut document.pending_finals {
        supersede_pending_final(pending);
    }
    document.final_emission_committed = false;
    document.committed_final = None;
}

fn supersede_uncommitted_task(document: &mut TaskEvidenceDocument) {
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.plan.clear();
    document.active_step_id = None;
    document.command_receipts.clear();
    document.validation_receipts.clear();
    document.generated_artifact_requirements.clear();
    document.generated_artifact_hashes.clear();
    document.latest_generated_artifact_hashes.clear();
    document.verify_plan_epoch = None;
    document.validation_epoch = None;
    document.desktop_activation_receipt = None;
    document.automatic_plan_attempt_epoch = None;
    document.completion = None;
    document.phase = TaskPhase::Unclassified;
    document.outcome = None;
    document.classification = None;
    document.investigation_checkpoint_hash = None;
    document.accepted_receipt_hashes.clear();
    document.accepted_closure = None;
    document.review_findings.clear();
    document.latest_review_finding_revision = None;
    document.actionable_findings.clear();
    document.latest_actionable_finding_revision = None;
    document.clean_review_hash = None;
    document.prepared_review = None;
    document.review_attempt_failures.clear();
    document.closure_fingerprint = None;
    document.incomplete_occurrences.clear();
    document.mutation_targets.clear();
    for state in document.root_baselines.values_mut() {
        state.explicit_target_snapshots.clear();
    }
    for state in document.known_roots.values_mut() {
        state.explicit_target_snapshots.clear();
    }
    for pending in &mut document.pending_finals {
        supersede_pending_final(pending);
    }
    document.final_emission_committed = false;
    document.committed_final = None;
}

fn fail_closed_for_storage(document: &mut TaskEvidenceDocument, reason: &str) {
    document.outcome = None;
    document.accepted_closure = None;
    document.prepared_review = None;
    document.clean_review_hash = None;
    document.closure_fingerprint = None;
    document.final_emission_committed = false;
    document.committed_final = None;
    invalidate_final_reservations(document);
    if document.classification.is_some() {
        document.phase = TaskPhase::Fixing;
    } else {
        document.phase = TaskPhase::Unclassified;
    }
    let epoch = document.evidence_epoch;
    upsert_risk(
        document,
        EvidenceRisk {
            id: "task-evidence-storage".to_string(),
            description: reason.to_string(),
            source: "task_evidence_storage".to_string(),
            blocking: true,
            resolved: false,
            epoch,
        },
    );
}

fn migrate_document(document: &mut TaskEvidenceDocument) {
    let previous_schema_version = document.schema_version;
    if previous_schema_version < 3 {
        let legacy_mutation = !document.edit_receipts.is_empty()
            || document
                .command_receipts
                .iter()
                .any(|receipt| receipt.possible_mutation);
        document.mutation_revision = document.evidence_epoch.max(u64::from(legacy_mutation));
        document.phase = TaskPhase::Unclassified;
        document.outcome = None;
        document.accepted_closure = None;
        document.final_emission_committed = false;
    }
    if previous_schema_version < 4 {
        for receipt in &mut document.validation_receipts {
            receipt.accepted_proof = false;
        }
        if matches!(document.phase, TaskPhase::Ready | TaskPhase::Reviewing) {
            document.phase = if document.classification.is_some() {
                TaskPhase::Fixing
            } else {
                TaskPhase::Unclassified
            };
        }
        document.outcome = None;
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.final_emission_committed = false;
        document.committed_final = None;
    }
    if previous_schema_version < 5 {
        for receipt in &mut document.validation_receipts {
            receipt.accepted_proof = false;
        }
        document.command_receipts.clear();
        document.evidence_epoch = document.evidence_epoch.saturating_add(1);
        document.verify_plan_epoch = None;
        document.validation_epoch = None;
        document.desktop_activation_receipt = None;
        document.automatic_plan_attempt_epoch = None;
        document.generated_artifact_hashes.clear();
        document.latest_generated_artifact_hashes.clear();
        document.latest_file_hashes.clear();
        for step in &mut document.plan {
            if step.status == StepStatus::Passed {
                step.status = StepStatus::Implemented;
            }
            step.validation_receipt_ids.clear();
        }
        for requirement in &mut document.generated_artifact_requirements {
            requirement.validation_receipt_ids.clear();
        }
        document.investigation_checkpoint_hash = None;
        document.accepted_receipt_hashes.clear();
        document.accepted_evidence_revision = 0;
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.review_attempt_failures.clear();
        document.closure_fingerprint = None;
        document.incomplete_occurrences.clear();
        document.outcome = None;
        document.phase = if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        document.final_emission_committed = false;
        document.committed_final = None;
        document.task_generation = document.task_generation.saturating_add(1);
        for pending in &mut document.pending_finals {
            supersede_pending_final(pending);
        }
    }
    if previous_schema_version < 6 {
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.review_attempt_failures.clear();
        document.closure_fingerprint = None;
        document.incomplete_occurrences.clear();
        document.outcome = None;
        document.phase = if document
            .classification
            .as_ref()
            .is_some_and(|classification| {
                classification.exhaustive && document.investigation_checkpoint_hash.is_none()
            }) {
            TaskPhase::Investigating
        } else if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        document.final_emission_committed = false;
        document.committed_final = None;
        if previous_schema_version >= 5 {
            document.task_generation = document.task_generation.saturating_add(1);
        }
        invalidate_final_reservations(document);
    }
    if previous_schema_version < 7 {
        document.root_baselines = document.known_roots.clone();
        document.task_changed_paths.clear();
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.review_attempt_failures.clear();
        document.closure_fingerprint = None;
        document.incomplete_occurrences.clear();
        document.outcome = None;
        document.phase = if document
            .classification
            .as_ref()
            .is_some_and(|classification| {
                classification.exhaustive && document.investigation_checkpoint_hash.is_none()
            }) {
            TaskPhase::Investigating
        } else if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        document.final_emission_committed = false;
        document.committed_final = None;
        if previous_schema_version >= 6 {
            document.task_generation = document.task_generation.saturating_add(1);
        }
        invalidate_final_reservations(document);
    }
    if previous_schema_version < 8 {
        document.root_baselines = document.known_roots.clone();
        document.task_changed_paths = if document.mutation_revision > 0 {
            document
                .known_roots
                .keys()
                .cloned()
                .map(|root| (root, BTreeSet::from([".".to_string()])))
                .collect()
        } else {
            BTreeMap::new()
        };
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.review_attempt_failures.clear();
        document.closure_fingerprint = None;
        document.incomplete_occurrences.clear();
        document.outcome = None;
        document.phase = if document
            .classification
            .as_ref()
            .is_some_and(|classification| {
                classification.exhaustive && document.investigation_checkpoint_hash.is_none()
            }) {
            TaskPhase::Investigating
        } else if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        document.final_emission_committed = false;
        document.committed_final = None;
        if previous_schema_version >= 7 {
            document.task_generation = document.task_generation.saturating_add(1);
        }
        invalidate_final_reservations(document);
    }
    if previous_schema_version < 9 {
        document.mutation_targets.clear();
        for state in document.root_baselines.values_mut() {
            state.explicit_target_snapshots.clear();
        }
        for state in document.known_roots.values_mut() {
            state.explicit_target_snapshots.clear();
        }
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.review_attempt_failures.clear();
        document.closure_fingerprint = None;
        document.incomplete_occurrences.clear();
        document.outcome = None;
        document.phase = if document
            .classification
            .as_ref()
            .is_some_and(|classification| {
                classification.exhaustive && document.investigation_checkpoint_hash.is_none()
            }) {
            TaskPhase::Investigating
        } else if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        document.final_emission_committed = false;
        document.committed_final = None;
        if previous_schema_version >= 8 {
            document.task_generation = document.task_generation.saturating_add(1);
        }
        invalidate_final_reservations(document);
    }
    if previous_schema_version < 11 {
        if let Some(committed) = document.committed_final.as_mut() {
            // Older schemas marked the transaction complete after the item batch,
            // before the durable TurnComplete append. Re-open that terminal fence.
            committed.completed = false;
            committed.terminal_event_staged = false;
        }
        hydrate_final_outbox(document);
    }
    if previous_schema_version < 12 && !document.final_emission_committed {
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
        document.review_attempt_failures.clear();
        document.closure_fingerprint = None;
        document.incomplete_occurrences.clear();
        document.outcome = None;
        document.phase = if document
            .classification
            .as_ref()
            .is_some_and(|classification| {
                classification.exhaustive && document.investigation_checkpoint_hash.is_none()
            }) {
            TaskPhase::Investigating
        } else if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        invalidate_final_reservations(document);
    }
    if document.phase == TaskPhase::Closing {
        document.phase = TaskPhase::Fixing;
        document.outcome = None;
        document.accepted_closure = None;
        document.clean_review_hash = None;
    }
    document.schema_version = TASK_EVIDENCE_SCHEMA_VERSION;
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
    document.next_validation_receipt_sequence =
        document
            .next_validation_receipt_sequence
            .max(next_sequence_after_ids(
                document
                    .validation_receipts
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
    let (duplicate_validation_indices, duplicate_validation_ids) = duplicate_receipt_indices(
        document
            .validation_receipts
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_validation_indices {
        let id = next_receipt_id("validation", &mut document.next_validation_receipt_sequence);
        document.validation_receipts[index].id = id;
    }
    if !duplicate_validation_ids.is_empty() {
        for step in &mut document.plan {
            step.validation_receipt_ids
                .retain(|id| !duplicate_validation_ids.contains(id));
        }
        for requirement in &mut document.generated_artifact_requirements {
            requirement
                .validation_receipt_ids
                .retain(|id| !duplicate_validation_ids.contains(id));
        }
    }
    if document.latest_generated_artifact_hashes.is_empty() {
        document.latest_generated_artifact_hashes = document.generated_artifact_hashes.clone();
    }
    let mut used_ids = BTreeSet::new();
    let mut duplicate_step_ids = BTreeSet::new();
    for (index, step) in document.plan.iter_mut().enumerate() {
        if !used_ids.insert(step.id.clone()) {
            duplicate_step_ids.insert(step.id.clone());
            step.id = unique_step_id(&step.id, index, &mut used_ids);
            step.validation_receipt_ids.clear();
            if step.status == StepStatus::Passed {
                step.status = StepStatus::Implemented;
            }
        }
    }
    sync_plan_structure_state(document, &duplicate_step_ids);
    promote_steps_with_fresh_evidence(document);
    repair_task_lifecycle_invariants(document);
}

fn hydrate_final_outbox(document: &mut TaskEvidenceDocument) {
    let fallback_completion = ready_outcome_completion_gate(document, None);
    for pending in &mut document.pending_finals {
        let task_generation = pending.task_generation;
        let evidence_revision = pending.evidence_revision;
        hydrate_pending_final_emission(pending, task_generation, evidence_revision);
    }
    if let Some(committed) = document.committed_final.as_mut()
        && committed.emission_key.is_empty()
        && let Some(pending) = document.pending_finals.iter().find(|pending| {
            pending.task_generation == committed.task_generation
                && pending.turn_id == committed.turn_id
                && pending.item_id == committed.item_id
                && !pending.emission_key.is_empty()
        })
    {
        committed.emission_key = pending.emission_key.clone();
    }
    if let Some(committed) = document.committed_final.as_mut()
        && committed.terminal_event.is_none()
        && let Some(pending) = document.pending_finals.iter().find(|pending| {
            pending.task_generation == committed.task_generation
                && pending.turn_id == committed.turn_id
                && pending.item_id == committed.item_id
                && !pending.emission_items.is_empty()
        })
    {
        committed.terminal_event = Some(fallback_final_terminal_event(
            &committed.turn_id,
            &pending.emission_items,
            fallback_completion,
        ));
        committed.terminal_event_staged = false;
    }
}

fn repair_task_lifecycle_invariants(document: &mut TaskEvidenceDocument) {
    hydrate_final_outbox(document);
    let task_generation = document.task_generation;
    for pending in &mut document.pending_finals {
        if pending.task_generation != task_generation {
            supersede_pending_final(pending);
        }
    }
    if document.classification.is_none() {
        document.phase = TaskPhase::Unclassified;
        document.outcome = None;
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
    } else if document
        .classification
        .as_ref()
        .is_some_and(|classification| classification.exhaustive)
        && document.investigation_checkpoint_hash.is_none()
    {
        document.phase = TaskPhase::Investigating;
        document.outcome = None;
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
    } else if document.phase == TaskPhase::Unclassified {
        document.phase = TaskPhase::Fixing;
    }
    let closure_binding_current = accepted_closure_binding_is_current(document);
    let invalid_phase = match document.phase {
        TaskPhase::Ready => {
            document.outcome.is_none()
                || (document.classification.is_some()
                    && (!closure_binding_current
                        || document.accepted_closure.as_ref().is_none_or(|closure| {
                            closure.terminal_outcome != document.outcome
                                || (closure.terminal_outcome == Some(TaskOutcome::Passed)
                                    && !closure.missing_requirement_ids.is_empty())
                        })
                        || document.accepted_closure.as_ref().is_some_and(|closure| {
                            closure.review_required && document.clean_review_hash.is_none()
                        })))
        }
        TaskPhase::Reviewing => {
            document.outcome.is_some()
                || !closure_binding_current
                || document
                    .accepted_closure
                    .as_ref()
                    .is_none_or(|closure| !closure.review_required)
        }
        TaskPhase::Closing => true,
        TaskPhase::Unclassified | TaskPhase::Investigating | TaskPhase::Fixing => {
            document.outcome.is_some()
        }
    };
    if invalid_phase {
        document.phase = if document.classification.is_some() {
            TaskPhase::Fixing
        } else {
            TaskPhase::Unclassified
        };
        document.outcome = None;
        document.accepted_closure = None;
        document.prepared_review = None;
        document.clean_review_hash = None;
    }
    if document.phase != TaskPhase::Reviewing {
        document.prepared_review = None;
    }
    let committed_is_current = document.committed_final.as_ref().is_some_and(|committed| {
        committed.task_generation == document.task_generation
            && committed.evidence_revision == document.accepted_evidence_revision
            && !committed.emission_key.is_empty()
            && matches!(
                committed.terminal_event.as_ref(),
                Some(EventMsg::TurnComplete(event)) if event.turn_id == committed.turn_id
            )
            && document.active_turn_id.as_deref() == Some(committed.turn_id.as_str())
            && document.pending_finals.iter().any(|pending| {
                pending.task_generation == committed.task_generation
                    && pending.turn_id == committed.turn_id
                    && pending.item_id == committed.item_id
                    && !pending.superseded
                    && pending.persisted
                    && !pending.emission_items.is_empty()
                    && pending.emission_key == committed.emission_key
                    && (!committed.completed
                        || (committed.terminal_event_staged
                            && pending.externally_emitted
                            && pending.externally_completed))
            })
    });
    if !committed_is_current {
        document.final_emission_committed = false;
        document.committed_final = None;
    } else {
        document.final_emission_committed = true;
    }
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

fn normalize_classification(
    classification: TaskClassification,
    cwd: &Path,
) -> Result<TaskClassification, String> {
    let mut risk_domains = BTreeSet::new();
    for risk in classification.risk_domains {
        risk_domains.insert(normalize_risk_domain(&risk)?);
    }
    let mut supported_non_git_roots = BTreeSet::new();
    for root in classification.supported_non_git_roots {
        let root = canonicalize_mutation_target(cwd, Path::new(&root))?;
        if find_git_repo_root(&root).is_some() {
            return Err(format!(
                "supported non-Git root `{}` is already owned by a Git repository",
                root.display()
            ));
        }
        supported_non_git_roots.insert(root.to_string_lossy().into_owned());
    }
    Ok(TaskClassification {
        exhaustive: classification.exhaustive,
        risk_domains,
        supported_non_git_roots,
    })
}

fn normalize_risk_domain(risk: &str) -> Result<String, String> {
    let normalized = risk
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_")
        .replace(' ', "_");
    let normalized = match normalized.as_str() {
        "highrisk" => "high_risk",
        "executionsafety" => "execution_safety",
        "uncertainwiring" => "uncertain_wiring",
        "install" => "installation",
        value => value,
    };
    if matches!(
        normalized,
        "broad"
            | "high_risk"
            | "protocol"
            | "schema"
            | "execution_safety"
            | "persistence"
            | "lifecycle"
            | "publish"
            | "installation"
            | "uncertain_wiring"
    ) {
        Ok(normalized.to_string())
    } else {
        Err(format!("unknown task risk domain `{risk}`"))
    }
}

fn canonicalize_existing_path(path: &Path) -> Result<PathBuf, String> {
    dunce::canonicalize(path).map_err(|err| {
        format!(
            "could not canonicalize mutation owner `{}`: {err}",
            path.display()
        )
    })
}

pub(crate) fn canonicalize_mutation_target(cwd: &Path, path: &Path) -> Result<PathBuf, String> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = lexical_normalize_path(&joined)?;
    let existing = normalized
        .ancestors()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            format!(
                "mutation target `{}` has no resolvable existing ancestor",
                normalized.display()
            )
        })?;
    let canonical_existing = dunce::canonicalize(existing).map_err(|err| {
        format!(
            "could not resolve mutation target ancestor `{}`: {err}",
            existing.display()
        )
    })?;
    let suffix = normalized.strip_prefix(existing).map_err(|_| {
        format!(
            "mutation target `{}` escaped its resolved ancestor",
            normalized.display()
        )
    })?;
    lexical_normalize_path(&canonical_existing.join(suffix))
}

fn lexical_normalize_path(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "mutation target `{}` traverses above its filesystem root",
                        path.display()
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if !normalized.is_absolute() {
        return Err(format!(
            "mutation target `{}` did not resolve to an absolute path",
            path.display()
        ));
    }
    Ok(normalized)
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().to_ascii_lowercase();
        let root = root.to_string_lossy().to_ascii_lowercase();
        path == root
            || path
                .strip_prefix(&root)
                .is_some_and(|suffix| suffix.starts_with('\\') || suffix.starts_with('/'))
    }
    #[cfg(not(windows))]
    {
        path == root || path.starts_with(root)
    }
}

fn deepest_containing_root(target: &str, roots: &[String]) -> Option<String> {
    roots
        .iter()
        .filter(|root| path_is_within(Path::new(target), Path::new(root)))
        .max_by_key(|root| Path::new(root).components().count())
        .cloned()
}

async fn snapshot_roots(roots: &[String]) -> BTreeMap<String, RepositoryState> {
    let mut states = BTreeMap::new();
    for root in roots {
        let state = snapshot_repository_state(Path::new(root)).await;
        states.insert(state.root.clone(), state);
    }
    states
}

async fn snapshot_non_git_roots(
    roots: &BTreeSet<String>,
) -> BTreeMap<String, ExplicitMutationTargetState> {
    let mut states = BTreeMap::new();
    for root in roots {
        states.insert(
            root.clone(),
            snapshot_explicit_mutation_target(Path::new(root)).await,
        );
    }
    states
}

fn find_git_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .and_then(|candidate| dunce::canonicalize(candidate).ok())
}

async fn snapshot_explicit_mutation_target(path: &Path) -> ExplicitMutationTargetState {
    let search_from = if path.is_dir() {
        Some(path)
    } else {
        path.ancestors().find(|candidate| candidate.is_dir())
    };
    let owner_root = search_from
        .and_then(find_git_repo_root)
        .map(|root| root.to_string_lossy().into_owned());
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return ExplicitMutationTargetState {
                owner_root,
                kind: "absent".to_string(),
                sha1: None,
                exists: false,
                read_error: None,
            };
        }
        Err(err) => {
            return ExplicitMutationTargetState {
                owner_root,
                kind: "unreadable".to_string(),
                sha1: None,
                exists: path.exists(),
                read_error: Some(format!("{:?}", err.kind())),
            };
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        if owner_root
            .as_deref()
            .is_some_and(|root| Path::new(root) == path)
        {
            return ExplicitMutationTargetState {
                owner_root,
                kind: "directory".to_string(),
                sha1: None,
                exists: true,
                read_error: None,
            };
        }
        let directory = path.to_path_buf();
        return match tokio::task::spawn_blocking(move || hash_explicit_directory(&directory)).await
        {
            Ok(Ok(sha1)) => ExplicitMutationTargetState {
                owner_root,
                kind: "directory".to_string(),
                sha1: Some(sha1),
                exists: true,
                read_error: None,
            },
            Ok(Err(err)) => ExplicitMutationTargetState {
                owner_root,
                kind: "directory".to_string(),
                sha1: None,
                exists: true,
                read_error: Some(format!("{:?}", err.kind())),
            },
            Err(_) => ExplicitMutationTargetState {
                owner_root,
                kind: "directory".to_string(),
                sha1: None,
                exists: true,
                read_error: Some("JoinError".to_string()),
            },
        };
    }
    if file_type.is_symlink() {
        return match tokio::fs::read_link(path).await {
            Ok(target) => ExplicitMutationTargetState {
                owner_root,
                kind: "symlink".to_string(),
                sha1: Some(sha1_hex(target.to_string_lossy().as_bytes())),
                exists: true,
                read_error: None,
            },
            Err(err) => ExplicitMutationTargetState {
                owner_root,
                kind: "symlink".to_string(),
                sha1: None,
                exists: true,
                read_error: Some(format!("{:?}", err.kind())),
            },
        };
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => ExplicitMutationTargetState {
            owner_root,
            kind: if file_type.is_file() {
                "file".to_string()
            } else {
                "other".to_string()
            },
            sha1: Some(sha1_hex(&bytes)),
            exists: true,
            read_error: None,
        },
        Err(err) => ExplicitMutationTargetState {
            owner_root,
            kind: if file_type.is_file() {
                "file".to_string()
            } else {
                "other".to_string()
            },
            sha1: None,
            exists: true,
            read_error: Some(format!("{:?}", err.kind())),
        },
    }
}

fn hash_explicit_directory(root: &Path) -> io::Result<String> {
    let mut pending_directories = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(directory) = pending_directories.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            paths.push(path.clone());
            if paths.len() > MAX_EXPLICIT_DIRECTORY_ENTRIES {
                return Err(io::Error::other(format!(
                    "explicit directory exceeds {MAX_EXPLICIT_DIRECTORY_ENTRIES} entries"
                )));
            }
            if std::fs::symlink_metadata(&path)?.file_type().is_dir() {
                pending_directories.push(path);
            }
        }
    }
    paths.sort_by(|left, right| {
        normalize_slashes(&left.strip_prefix(root).unwrap_or(left).to_string_lossy()).cmp(
            &normalize_slashes(&right.strip_prefix(root).unwrap_or(right).to_string_lossy()),
        )
    });

    let mut hasher = Sha1::new();
    hasher.update(b"directory\0");
    let mut buffer = [0_u8; 64 * 1024];
    for path in paths {
        let relative = normalize_slashes(
            &path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_string_lossy(),
        );
        let metadata = std::fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        if file_type.is_dir() {
            hasher.update(b"directory\0");
        } else if file_type.is_symlink() {
            hasher.update(b"symlink\0");
            hasher.update(std::fs::read_link(&path)?.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        } else if file_type.is_file() {
            hasher.update(b"file\0");
            let mut file = std::fs::File::open(&path)?;
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            hasher.update(b"\0");
        } else {
            hasher.update(b"other\0");
            hasher.update(metadata.len().to_le_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn absent_repository_state(root: &Path) -> RepositoryState {
    RepositoryState {
        root: root.to_string_lossy().into_owned(),
        available: true,
        error: None,
        head: None,
        tracked_diff_hash: sha1_hex(&[]),
        staged_diff_hash: sha1_hex(&[]),
        untracked_hash: canonical_hash(&BTreeMap::<String, Option<String>>::new()),
        dirty_paths: BTreeSet::new(),
        dirty_file_snapshots: BTreeMap::new(),
        dirty_path_states: BTreeMap::new(),
        explicit_target_snapshots: BTreeMap::new(),
    }
}

fn unknown_repository_baseline(root: &Path) -> RepositoryState {
    let mut state = absent_repository_state(root);
    state.available = false;
    state.error = Some("repository baseline predates exact root snapshots".to_string());
    state
}

async fn snapshot_repository_state(repo_root: &Path) -> RepositoryState {
    let canonical_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let mut errors = Vec::new();
    let reported_root = match git_stdout_checked(repo_root, &["rev-parse", "--show-toplevel"]).await
    {
        Ok(root) => match dunce::canonicalize(root) {
            Ok(root) if root == canonical_root => Some(root),
            Ok(root) => {
                errors.push(format!(
                    "Git reported root `{}` instead of `{}`",
                    root.display(),
                    canonical_root.display()
                ));
                None
            }
            Err(err) => {
                errors.push(format!("could not canonicalize reported Git root: {err}"));
                None
            }
        },
        Err(err) => {
            errors.push(err);
            None
        }
    };
    let head = match git_stdout_checked(repo_root, &["rev-parse", "HEAD"]).await {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) => {
            errors.push("Git HEAD was empty".to_string());
            None
        }
        Err(err) => {
            errors.push(err);
            None
        }
    };
    let tracked = match git_bytes_checked(repo_root, &["diff", "--no-ext-diff", "--binary"]).await {
        Ok(bytes) => bytes,
        Err(err) => {
            errors.push(err);
            Vec::new()
        }
    };
    let staged = match git_bytes_checked(
        repo_root,
        &["diff", "--cached", "--no-ext-diff", "--binary"],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(err) => {
            errors.push(err);
            Vec::new()
        }
    };
    let untracked = match git_bytes_checked(
        repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(err) => {
            errors.push(err);
            Vec::new()
        }
    };
    let mut untracked_inventory = BTreeMap::new();
    for raw_path in untracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let Ok(path) = std::str::from_utf8(raw_path) else {
            errors.push("Git returned a non-UTF-8 untracked path".to_string());
            continue;
        };
        let content_hash = match tokio::fs::read(repo_root.join(path)).await {
            Ok(bytes) => Some(sha1_hex(&bytes)),
            Err(err) => {
                errors.push(format!("could not read untracked path `{path}`: {err}"));
                None
            }
        };
        untracked_inventory.insert(normalize_slashes(path), content_hash);
    }
    let dirty_path_statuses = match git_dirty_path_statuses_checked(repo_root).await {
        Ok(statuses) => statuses,
        Err(err) => {
            errors.push(err);
            BTreeMap::new()
        }
    };
    let dirty_paths = dirty_path_statuses.keys().cloned().collect::<BTreeSet<_>>();
    let mut dirty_file_snapshots = BTreeMap::new();
    let mut dirty_path_states = BTreeMap::new();
    for path in &dirty_paths {
        let snapshot = snapshot_file(repo_root, path).await;
        if let Some(read_error) = snapshot.read_error.as_ref() {
            errors.push(format!(
                "could not snapshot dirty path `{path}`: {read_error}"
            ));
        }
        dirty_file_snapshots.insert(path.clone(), snapshot);
        let worktree = snapshot_explicit_mutation_target(&repo_root.join(path)).await;
        if let Some(read_error) = worktree.read_error.as_ref() {
            errors.push(format!(
                "could not snapshot dirty path metadata `{path}`: {read_error}"
            ));
        }
        let index_hash =
            match git_bytes_checked(repo_root, &["ls-files", "--stage", "-z", "--", path]).await {
                Ok(bytes) => sha1_hex(&bytes),
                Err(err) => {
                    errors.push(format!("could not snapshot Git index path `{path}`: {err}"));
                    String::new()
                }
            };
        dirty_path_states.insert(
            path.clone(),
            GitDirtyPathState {
                status: dirty_path_statuses.get(path).cloned().unwrap_or_default(),
                index_hash,
                worktree,
            },
        );
    }
    RepositoryState {
        root: canonical_root.to_string_lossy().into_owned(),
        available: errors.is_empty() && reported_root.is_some(),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
        head,
        tracked_diff_hash: sha1_hex(&tracked),
        staged_diff_hash: sha1_hex(&staged),
        untracked_hash: canonical_hash(&untracked_inventory),
        dirty_paths,
        dirty_file_snapshots,
        dirty_path_states,
        explicit_target_snapshots: BTreeMap::new(),
    }
}

async fn task_changed_paths_from_baseline(
    repo_root: &Path,
    baseline: Option<&RepositoryState>,
    current: &RepositoryState,
) -> BTreeSet<String> {
    let mut changed = BTreeSet::new();
    let Some(baseline) = baseline else {
        changed.extend(current.dirty_paths.iter().cloned());
        changed.extend(
            current
                .explicit_target_snapshots
                .keys()
                .map(|target| mutation_target_path_within_root(repo_root, target)),
        );
        if !current.available {
            changed.insert(".".to_string());
        }
        return changed;
    };
    if !baseline.available || !current.available {
        changed.insert(".".to_string());
        return changed;
    }
    let aggregate_diff_changed = baseline.tracked_diff_hash != current.tracked_diff_hash
        || baseline.staged_diff_hash != current.staged_diff_hash
        || baseline.untracked_hash != current.untracked_hash;
    let mut attributed_diff_change = false;
    for path in baseline
        .dirty_paths
        .union(&current.dirty_paths)
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        if baseline.dirty_path_states.get(&path) != current.dirty_path_states.get(&path)
            || baseline.dirty_file_snapshots.get(&path) != current.dirty_file_snapshots.get(&path)
        {
            attributed_diff_change = true;
            changed.insert(path);
        }
    }
    for target in baseline
        .explicit_target_snapshots
        .keys()
        .chain(current.explicit_target_snapshots.keys())
        .collect::<BTreeSet<_>>()
    {
        if baseline.explicit_target_snapshots.get(target)
            != current.explicit_target_snapshots.get(target)
        {
            changed.insert(mutation_target_path_within_root(repo_root, target));
        }
    }
    if baseline.head != current.head {
        let Some(baseline_head) = baseline.head.as_deref() else {
            changed.insert(".".to_string());
            return changed;
        };
        let Some(current_head) = current.head.as_deref() else {
            changed.insert(".".to_string());
            return changed;
        };
        let range = format!("{baseline_head}..{current_head}");
        match git_bytes_checked(
            repo_root,
            &["diff", "--name-only", "-z", "--no-renames", &range],
        )
        .await
        {
            Ok(paths) => {
                for raw_path in paths
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                {
                    match std::str::from_utf8(raw_path) {
                        Ok(path) => {
                            changed.insert(normalize_slashes(path));
                        }
                        Err(_) => {
                            changed.insert(".".to_string());
                        }
                    }
                }
            }
            Err(_) => {
                changed.insert(".".to_string());
            }
        }
    }
    if aggregate_diff_changed && !attributed_diff_change {
        changed.insert(".".to_string());
    }
    changed
}

fn mutation_target_path_within_root(repo_root: &Path, target: &str) -> String {
    let target = Path::new(target);
    match target.strip_prefix(repo_root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
        Ok(relative) => normalize_slashes(&relative.to_string_lossy()),
        Err(_) => ".".to_string(),
    }
}

async fn git_bytes_checked(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .map_err(|err| format!("could not execute `git {}`: {err}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "`git {}` failed with {}{}",
            args.join(" "),
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ));
    }
    Ok(output.stdout)
}

async fn git_stdout_checked(repo_root: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git_bytes_checked(repo_root, args).await?)
        .map(|value| value.trim().to_string())
        .map_err(|err| format!("`git {}` returned non-UTF-8 output: {err}", args.join(" ")))
}

async fn git_dirty_paths(repo_root: &Path) -> BTreeSet<String> {
    git_dirty_path_statuses_checked(repo_root)
        .await
        .map(|statuses| statuses.into_keys().collect())
        .unwrap_or_default()
}

async fn git_dirty_path_statuses_checked(
    repo_root: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await;
    let output = output.map_err(|err| format!("could not execute `git status`: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git status` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_git_porcelain_path_statuses_checked(&output.stdout)
}

fn parse_git_porcelain_path_statuses_checked(
    output: &[u8],
) -> Result<BTreeMap<String, String>, String> {
    let mut paths = BTreeMap::new();
    let mut records = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            return Err("Git status returned an invalid porcelain record".to_string());
        }
        let path = std::str::from_utf8(&record[3..])
            .map_err(|_| "Git status returned a non-UTF-8 path".to_string())?;
        if path.is_empty() {
            return Err("Git status returned an empty path".to_string());
        }
        let status = std::str::from_utf8(&record[..2])
            .map_err(|_| "Git status returned a non-UTF-8 status".to_string())?
            .to_string();
        paths.insert(normalize_slashes(path), status.clone());
        if record[..2]
            .iter()
            .any(|status| matches!(*status, b'R' | b'C'))
        {
            let original_path = records
                .next()
                .ok_or_else(|| "Git status omitted a rename source path".to_string())?;
            let original_path = std::str::from_utf8(original_path)
                .map_err(|_| "Git status returned a non-UTF-8 rename path".to_string())?;
            if original_path.is_empty() {
                return Err("Git status returned an empty rename path".to_string());
            }
            paths.insert(normalize_slashes(original_path), status);
        }
    }
    Ok(paths)
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

fn normalize_requested_status(
    requested: &StepStatus,
    previous: Option<&StepStatus>,
    material_step_change: bool,
) -> StepStatus {
    match requested {
        StepStatus::Passed | StepStatus::Completed => {
            if !material_step_change && previous == Some(&StepStatus::Passed) {
                StepStatus::Passed
            } else {
                StepStatus::Implemented
            }
        }
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
    document
        .generated_artifact_requirements
        .retain(|requirement| requirement.source == "verify_local");
    document.risks.retain(|risk| risk.source != "plan");
    let mut requirements = Vec::new();
    let mut risks = Vec::new();
    for step in &document.plan {
        for (index, path) in step.generated_artifacts.iter().enumerate() {
            requirements.push(GeneratedArtifactRequirement {
                id: format!("plan:{}:artifact:{index}", step.id),
                step_id: Some(step.id.clone()),
                path: Some(normalize_slashes(path)),
                validation_command: Vec::new(),
                source: "plan".to_string(),
                validation_receipt_ids: Vec::new(),
            });
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

fn rebuild_verifier_requirements(document: &mut TaskEvidenceDocument, payload: Option<&Value>) {
    document
        .generated_artifact_requirements
        .retain(|requirement| requirement.source != "verify_local");
    let Some(planned) = payload
        .and_then(|value| value.get("planned"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for item in planned {
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !matches!(kind, "surface_validation" | "surface_regen") {
            continue;
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("surface-validation")
            .to_string();
        let validation_command = item
            .get("command")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        document
            .generated_artifact_requirements
            .push(GeneratedArtifactRequirement {
                id,
                step_id: document.active_step_id.clone(),
                path: None,
                validation_command,
                source: "verify_local".to_string(),
                validation_receipt_ids: Vec::new(),
            });
    }
}

fn promote_steps_with_fresh_evidence(document: &mut TaskEvidenceDocument) {
    let mut demoted = true;
    while demoted {
        demoted = false;
        for index in 0..document.plan.len() {
            if document.plan[index].status == StepStatus::Passed
                && !step_has_fresh_evidence(document, index)
            {
                document.plan[index].status = StepStatus::Implemented;
                demoted = true;
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for index in 0..document.plan.len() {
            if !matches!(
                document.plan[index].status,
                StepStatus::Implemented | StepStatus::Completed | StepStatus::Passed
            ) {
                continue;
            }
            if step_has_fresh_evidence(document, index)
                && document.plan[index].status != StepStatus::Passed
            {
                document.plan[index].status = StepStatus::Passed;
                changed = true;
            }
        }
    }
    for risk in &mut document.risks {
        if let Some(step_id) = risk
            .id
            .strip_prefix("plan:")
            .and_then(|id| id.split(':').next())
        {
            risk.resolved = document
                .plan
                .iter()
                .any(|step| step.id == step_id && step.status == StepStatus::Passed);
        }
    }
}

fn step_has_fresh_evidence(document: &TaskEvidenceDocument, index: usize) -> bool {
    let step = &document.plan[index];
    if document.verify_plan_epoch != Some(document.evidence_epoch)
        || document.validation_epoch != Some(document.evidence_epoch)
    {
        return false;
    }
    if step.depends_on.iter().any(|dependency| {
        !document.plan.iter().any(|candidate| {
            candidate.id == *dependency
                && matches!(candidate.status, StepStatus::Passed | StepStatus::Skipped)
        })
    }) {
        return false;
    }
    if step.edit_paths.is_empty() || step.validation_receipt_ids.is_empty() {
        return false;
    }
    let validation = step
        .validation_receipt_ids
        .iter()
        .rev()
        .find_map(|receipt_id| {
            document.validation_receipts.iter().rev().find(|receipt| {
                receipt.id == *receipt_id
                    && validation_receipt_is_accepted(receipt, document.evidence_epoch)
            })
        });
    let Some(validation) = validation else {
        return false;
    };
    if step.edit_paths.iter().any(|path| {
        !validation
            .active_files
            .iter()
            .any(|active| active.read_error.is_none() && path_is_covered(path, &active.path))
    }) {
        return false;
    }
    if step.requires_desktop_activation
        && document
            .desktop_activation_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.epoch != document.evidence_epoch)
    {
        return false;
    }
    for artifact in &step.generated_artifacts {
        let normalized = normalize_slashes(artifact);
        if !generated_artifact_is_fresh(document, &normalized) {
            return false;
        }
    }
    true
}

fn edit_outcome_succeeded(outcome: &str) -> bool {
    outcome == "completed"
}

fn generated_artifact_is_fresh(document: &TaskEvidenceDocument, path: &str) -> bool {
    let normalized = normalize_slashes(path);
    let Some(baseline) = document.generated_artifact_hashes.get(&normalized) else {
        return false;
    };
    let Some(latest) = document.latest_generated_artifact_hashes.get(&normalized) else {
        return false;
    };
    baseline.exists
        && latest.exists
        && baseline.read_error.is_none()
        && latest.read_error.is_none()
        && baseline.sha1.is_some()
        && baseline.sha1 == latest.sha1
}

fn verifier_requirement_satisfied(
    requirement: &GeneratedArtifactRequirement,
    payload: Option<&Value>,
) -> bool {
    if requirement.validation_command.is_empty() {
        return false;
    }
    payload
        .and_then(|value| value.get("results"))
        .and_then(Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|result| {
                result.get("id").and_then(Value::as_str) == Some(requirement.id.as_str())
                    && result.get("status").and_then(Value::as_str) == Some("VERIFIED")
                    && result.get("exit_code").and_then(Value::as_i64) == Some(0)
                    && result.get("timed_out").and_then(Value::as_bool) == Some(false)
                    && result
                        .get("command")
                        .and_then(Value::as_array)
                        .is_some_and(|command| {
                            command.len() == requirement.validation_command.len()
                                && command.iter().zip(&requirement.validation_command).all(
                                    |(actual, expected)| actual.as_str() == Some(expected.as_str()),
                                )
                        })
            })
        })
}

fn pathless_requirement_has_fresh_receipt(
    document: &TaskEvidenceDocument,
    requirement: &GeneratedArtifactRequirement,
) -> bool {
    requirement
        .validation_receipt_ids
        .iter()
        .rev()
        .any(|receipt_id| {
            document.validation_receipts.iter().rev().any(|receipt| {
                receipt.id == *receipt_id
                    && receipt.epoch == document.evidence_epoch
                    && receipt.tool_success
                    && receipt.proof_bearing
                    && receipt.verdict.as_deref() == Some("VERIFIED")
                    && requirement
                        .step_id
                        .as_ref()
                        .is_none_or(|step_id| receipt.step_id.as_ref() == Some(step_id))
                    && verifier_requirement_satisfied(requirement, receipt.payload.as_ref())
            })
        })
}

fn command_display(command: &[String]) -> String {
    if command.is_empty() {
        return "<missing command>".to_string();
    }
    command
        .iter()
        .map(|argument| {
            if argument.is_empty() || argument.chars().any(char::is_whitespace) {
                format!("{argument:?}")
            } else {
                argument.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    let unresolved_steps = document
        .plan
        .iter()
        .enumerate()
        .filter(|(index, step)| {
            step.status != StepStatus::Skipped
                && (step.status != StepStatus::Passed || !step_has_fresh_evidence(document, *index))
        })
        .map(|(_, step)| format!("{} ({:?})", step.id, step.status))
        .collect::<Vec<_>>();
    if !unresolved_steps.is_empty() {
        partial.push(format!(
            "plan steps lack fresh passing evidence: {}",
            unresolved_steps.join(", ")
        ));
    }
    if document.verify_plan_epoch != Some(document.evidence_epoch) {
        partial.push("verify_local planning is missing or stale".to_string());
    }
    if document.validation_epoch != Some(document.evidence_epoch) {
        partial.push("proof-bearing verify_local validation is missing or stale".to_string());
    }
    if document
        .plan
        .iter()
        .any(|step| step.requires_desktop_activation)
        && document
            .desktop_activation_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.epoch != document.evidence_epoch)
    {
        blocked.push("required Desktop activation receipt is missing or stale".to_string());
    }
    for requirement in &document.generated_artifact_requirements {
        if let Some(path) = requirement.path.as_ref() {
            if !generated_artifact_is_fresh(document, path) {
                blocked.push(format!(
                    "required generated artifact is missing, unreadable, or stale: {path}"
                ));
            }
        } else if !pathless_requirement_has_fresh_receipt(document, requirement) {
            blocked.push(format!(
                "required verifier command lacks a matching fresh passing result: {}",
                command_display(&requirement.validation_command)
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

fn status_from_document(document: &TaskEvidenceDocument, message: &str) -> TaskLifecycleStatus {
    TaskLifecycleStatus {
        phase: document.phase,
        outcome: document.outcome,
        mutation_revision: document.mutation_revision,
        accepted_evidence_revision: document.accepted_evidence_revision,
        review_required: document
            .accepted_closure
            .as_ref()
            .is_some_and(|closure| closure.review_required && document.clean_review_hash.is_none())
            || document.phase == TaskPhase::Reviewing,
        closure_fingerprint: document.closure_fingerprint.clone(),
        incomplete_occurrences: document
            .closure_fingerprint
            .as_ref()
            .and_then(|fingerprint| document.incomplete_occurrences.get(fingerprint))
            .copied()
            .unwrap_or_default(),
        known_roots: document.known_roots.keys().cloned().collect(),
        unsupported_mutation_targets: document
            .unsupported_mutation_targets
            .iter()
            .cloned()
            .collect(),
        validation_receipt_ids: document
            .validation_receipts
            .iter()
            .filter(|receipt| validation_receipt_is_accepted(receipt, document.evidence_epoch))
            .map(|receipt| receipt.id.clone())
            .collect(),
        command_receipt_ids: document
            .command_receipts
            .iter()
            .filter(|receipt| {
                receipt.epoch == document.evidence_epoch
                    && receipt.exit_code == 0
                    && !receipt.timed_out
                    && command_receipt_is_runtime_evidence(receipt, document)
            })
            .map(|receipt| receipt.id.clone())
            .collect(),
        message: message.to_string(),
    }
}

fn descendant_coverage_from_document(
    document: &TaskEvidenceDocument,
) -> Result<DescendantTaskEvidenceCoverage, String> {
    let known_roots = document
        .known_roots
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let digest = canonical_hash(&serde_json::json!({
        "thread_id": document.thread_id,
        "task_generation": document.task_generation,
        "mutation_revision": document.mutation_revision,
        "known_roots": known_roots,
        "root_baselines": document.root_baselines,
        "observed_roots": document.observed_roots,
        "unsupported_mutation_targets": document.unsupported_mutation_targets,
        "pass_prohibited_mutation_targets": document.pass_prohibited_mutation_targets,
        "mutation_targets": document.mutation_targets,
        "non_git_root_snapshots": document.non_git_root_snapshots,
    }));
    Ok(DescendantTaskEvidenceCoverage {
        thread_id: document.thread_id.clone(),
        mutation_revision: document.mutation_revision,
        digest,
        known_roots,
        root_baselines: document.root_baselines.clone(),
        observed_roots: document.observed_roots.clone(),
        unsupported_mutation_targets: document.unsupported_mutation_targets.clone(),
        pass_prohibited_mutation_targets: document.pass_prohibited_mutation_targets.clone(),
        mutation_targets: document.mutation_targets.clone(),
        non_git_root_snapshots: document.non_git_root_snapshots.clone(),
    })
}

fn canonical_hash<T: Serialize>(value: &T) -> String {
    serde_json::to_vec(value)
        .map(|bytes| sha1_hex(&bytes))
        .unwrap_or_else(|_| sha1_hex(b"canonicalization-failed"))
}

fn canonical_validation_receipt_hash(receipt: &ValidationReceipt) -> String {
    let mut active_files = receipt.active_files.clone();
    active_files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut stale_reasons = receipt.stale_reasons.clone();
    stale_reasons.sort();
    stale_reasons.dedup();
    canonical_hash(&serde_json::json!({
        "step_id": receipt.step_id,
        "mode": receipt.mode,
        "verdict": receipt.verdict,
        "tool_success": receipt.tool_success,
        "proof_bearing": receipt.proof_bearing,
        "accepted_proof": receipt.accepted_proof,
        "active_files": active_files,
        "stale_reasons": stale_reasons,
        "payload": receipt.payload.as_ref().map(canonicalize_evidence_value),
    }))
}

fn canonical_command_receipt_hash(receipt: &CommandReceipt) -> String {
    canonical_hash(&serde_json::json!({
        "step_id": receipt.step_id,
        "command": receipt.command,
        "cwd": receipt.cwd,
        "exit_code": receipt.exit_code,
        "timed_out": receipt.timed_out,
        "possible_mutation": receipt.possible_mutation,
    }))
}

fn canonicalize_evidence_value(value: &Value) -> Value {
    canonicalize_evidence_value_at(value, None)
}

fn canonicalize_evidence_value_at(value: &Value, parent_key: Option<&str>) -> Value {
    match value {
        Value::Object(object) => {
            let mut canonical = BTreeMap::new();
            for (key, value) in object {
                let normalized_key = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized_key.as_str(),
                    "run_id"
                        | "recorded_at"
                        | "started_at"
                        | "completed_at"
                        | "timestamp"
                        | "duration"
                        | "duration_ms"
                        | "elapsed"
                        | "elapsed_ms"
                        | "log_path"
                        | "temp_path"
                ) {
                    continue;
                }
                canonical.insert(
                    normalized_key.clone(),
                    canonicalize_evidence_value_at(value, Some(&normalized_key)),
                );
            }
            serde_json::to_value(canonical).unwrap_or(Value::Null)
        }
        Value::Array(values) => {
            let mut canonical = values
                .iter()
                .map(|value| canonicalize_evidence_value_at(value, parent_key))
                .collect::<Vec<_>>();
            if !matches!(
                parent_key,
                Some("command" | "argv" | "args" | "arguments" | "prefix_rule")
            ) {
                canonical.sort_by(|left, right| {
                    serde_json::to_vec(left)
                        .unwrap_or_default()
                        .cmp(&serde_json::to_vec(right).unwrap_or_default())
                });
            }
            Value::Array(canonical)
        }
        value => value.clone(),
    }
}

fn normalize_review_paths(
    document: &TaskEvidenceDocument,
    values: &BTreeSet<String>,
    kind: &str,
) -> Result<BTreeSet<String>, String> {
    let repository_root = Path::new(&document.start.repository_root);
    let mut normalized = BTreeSet::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("{kind} entries cannot be empty"));
        }
        let path = Path::new(value);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            repository_root.join(path)
        };
        let canonical = canonicalize_existing_path(&candidate)
            .map_err(|err| format!("invalid {kind} path `{value}`: {err}"))?;
        let owner = document
            .known_roots
            .keys()
            .chain(document.supported_non_git_roots.iter())
            .map(PathBuf::from)
            .filter(|root| path_is_within(&canonical, root))
            .max_by_key(|root| root.components().count())
            .ok_or_else(|| {
                format!("{kind} path `{value}` is outside every registered task root")
            })?;
        let relative = canonical.strip_prefix(&owner).map_err(|_| {
            format!("{kind} path `{value}` could not be bound to its registered root")
        })?;
        normalized.insert(format!(
            "{}::{}",
            normalize_slashes(&owner.to_string_lossy()),
            normalize_slashes(&relative.to_string_lossy())
        ));
    }
    Ok(normalized)
}

fn review_paths_cover_task_changes(
    reviewed_paths: &BTreeSet<String>,
    task_changed_paths: &BTreeMap<String, BTreeSet<String>>,
) -> bool {
    task_changed_paths.iter().all(|(root, changed_paths)| {
        let root = normalize_slashes(root);
        changed_paths.iter().all(|changed_path| {
            let changed_path = normalize_slashes(changed_path);
            reviewed_paths.iter().any(|reviewed| {
                let Some((reviewed_root, reviewed_path)) = reviewed.split_once("::") else {
                    return false;
                };
                reviewed_root == root
                    && (reviewed_path.is_empty()
                        || reviewed_path == "."
                        || (changed_path != "." && path_is_covered(&changed_path, reviewed_path)))
            })
        })
    })
}

fn validation_receipt_is_accepted(receipt: &ValidationReceipt, epoch: u64) -> bool {
    receipt.epoch == epoch
        && receipt.accepted_proof
        && receipt.tool_success
        && receipt.proof_bearing
        && receipt.verdict.as_deref() == Some("VERIFIED")
        && receipt.stale_reasons.is_empty()
        && receipt
            .active_files
            .iter()
            .all(|snapshot| snapshot.read_error.is_none())
}

fn command_receipt_is_runtime_evidence(
    receipt: &CommandReceipt,
    document: &TaskEvidenceDocument,
) -> bool {
    if receipt.possible_mutation || receipt.command.is_empty() {
        return false;
    }
    let Ok(cwd_uri) = PathUri::parse(&receipt.cwd) else {
        return false;
    };
    let Ok(cwd) = cwd_uri.to_abs_path() else {
        return false;
    };
    if !document
        .known_roots
        .keys()
        .chain(document.supported_non_git_roots.iter())
        .any(|root| path_is_within(cwd.as_path(), Path::new(root)))
    {
        return false;
    }
    if !document.plan.is_empty()
        && receipt.step_id.as_ref().is_none_or(|step_id| {
            !document.plan.iter().any(|step| {
                step.id == *step_id
                    && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
            })
        })
    {
        return false;
    }
    let Some(tokens) = structurally_unwrap_command(&receipt.command) else {
        return false;
    };
    let Some((command_token, args)) = tokens.split_first() else {
        return false;
    };
    let command = command_token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command_token)
        .trim_end_matches(".exe");
    match command {
        "cargo" => match args.first().map(String::as_str) {
            Some("test" | "nextest") => !args.iter().any(|arg| arg == "--no-run"),
            Some("check" | "clippy" | "build") => true,
            _ => false,
        },
        "pytest" | "nextest" => true,
        "just" => args.first().is_some_and(|recipe| {
            matches!(
                recipe.as_str(),
                "test"
                    | "check"
                    | "clippy"
                    | "build"
                    | "smoke"
                    | "app-server-schema-check"
                    | "config-schema-check"
                    | "publish-local-codex-dry-run"
                    | "publish-local-codex-final"
            ) || recipe.ends_with("-test")
                || recipe.ends_with("-check")
                || recipe.ends_with("-smoke")
        }),
        "npm" | "pnpm" | "yarn" => args
            .first()
            .is_some_and(|script| matches!(script.as_str(), "test" | "build" | "lint" | "check")),
        "go" => args.first().is_some_and(|arg| arg == "test"),
        "dotnet" => args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "test" | "build")),
        "ruff" => args.first().is_some_and(|arg| arg == "check"),
        "dprint" => args.first().is_some_and(|arg| arg == "check"),
        "taplo" => args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "check" | "lint")),
        _ => false,
    }
}

fn structurally_unwrap_command(command: &[String]) -> Option<Vec<String>> {
    let mut tokens = command.to_vec();
    if tokens.len() == 1 {
        tokens = split_simple_command(&tokens[0])?;
    }
    let executable = tokens
        .first()?
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&tokens[0])
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    if matches!(
        executable.as_str(),
        "powershell" | "pwsh" | "bash" | "sh" | "zsh" | "cmd"
    ) {
        let flag_index = tokens.iter().position(|token| {
            matches!(
                token.to_ascii_lowercase().as_str(),
                "-c" | "-lc" | "-command" | "/c"
            )
        })?;
        let script = tokens.get(flag_index + 1..)?.join(" ");
        tokens = split_simple_command(&script)?;
    }
    if tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("env"))
    {
        tokens.remove(0);
        while tokens
            .first()
            .is_some_and(|token| token.starts_with('-') || token.contains('='))
        {
            tokens.remove(0);
        }
    }
    if tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("uv"))
        && tokens.get(1).is_some_and(|token| token == "run")
    {
        tokens.drain(0..2);
    }
    for token in &mut tokens {
        *token = token
            .trim_matches(|character: char| matches!(character, '"' | '\'' | '`'))
            .to_ascii_lowercase();
    }
    (!tokens.is_empty()).then_some(tokens)
}

fn split_simple_command(script: &str) -> Option<Vec<String>> {
    if script
        .chars()
        .any(|character| matches!(character, '\n' | '\r' | ';' | '|' | '&' | '`'))
    {
        return None;
    }
    let tokens = script
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|character: char| matches!(character, '"' | '\''))
                .to_string()
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    (!tokens.is_empty()).then_some(tokens)
}

fn normalize_semantic_ids(values: &BTreeSet<String>) -> BTreeSet<String> {
    values
        .iter()
        .filter_map(|value| {
            let normalized = value
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace('\\', "/")
                .to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect()
}

fn normalize_stable_ids(values: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    let normalized = normalize_semantic_ids(values);
    if normalized.len() != values.len() {
        return Err("missing requirement identifiers cannot be empty".to_string());
    }
    if let Some(invalid) = normalized.iter().find(|value| {
        !value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '.' | '/' | ':' | '@')
        })
    }) {
        return Err(format!(
            "invalid missing requirement identifier `{invalid}`; use a stable machine-readable ID"
        ));
    }
    Ok(normalized)
}

fn normalize_blocked_reasons(reasons: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    let mut normalized = BTreeSet::new();
    for reason in reasons {
        let reason = reason
            .trim()
            .to_ascii_lowercase()
            .replace('-', "_")
            .replace(' ', "_");
        if !matches!(
            reason.as_str(),
            "access_unavailable" | "external_state" | "unsupported_ownership" | "user_intervention"
        ) {
            return Err(format!(
                "unknown blocked reason `{reason}`; use an authoritative blocker category"
            ));
        }
        normalized.insert(reason);
    }
    Ok(normalized)
}

fn closure_runtime_requirements(
    document: &TaskEvidenceDocument,
) -> (BTreeSet<String>, BTreeSet<String>, bool) {
    let mut missing = BTreeSet::new();
    let mut actionable = BTreeSet::new();
    let mut authoritative_blocker = false;

    for step in &document.plan {
        if !matches!(step.status, StepStatus::Passed | StepStatus::Skipped) {
            missing.insert(format!("plan-step:{}", step.id));
        }
    }
    for requirement in &document.generated_artifact_requirements {
        let fresh = requirement.validation_receipt_ids.iter().any(|id| {
            document.validation_receipts.iter().any(|receipt| {
                receipt.id == *id
                    && validation_receipt_is_accepted(receipt, document.evidence_epoch)
            })
        });
        if !fresh {
            missing.insert(format!("generated-artifact:{}", requirement.id));
        }
    }
    for risk in document
        .risks
        .iter()
        .filter(|risk| !risk.resolved && risk.epoch == document.evidence_epoch)
    {
        let risk_id = format!("risk:{}", risk.id);
        missing.insert(risk_id.clone());
        if risk.source == "task_evidence_storage"
            || risk.description.to_ascii_lowercase().contains("unreadable")
            || risk
                .description
                .to_ascii_lowercase()
                .contains("unavailable")
        {
            authoritative_blocker = true;
        } else if risk.blocking {
            actionable.insert(risk_id);
        }
    }
    if document
        .latest_file_hashes
        .values()
        .any(|snapshot| snapshot.read_error.is_some())
    {
        missing.insert("task-file-unreadable".to_string());
        authoritative_blocker = true;
    }
    (missing, actionable, authoritative_blocker)
}

fn frozen_mutation_state_hash(document: &TaskEvidenceDocument) -> String {
    canonical_hash(&serde_json::json!({
        "git_roots": document.known_roots,
        "non_git_roots": document.non_git_root_snapshots,
    }))
}

fn apply_root_snapshots(
    document: &mut TaskEvidenceDocument,
    root_states: BTreeMap<String, RepositoryState>,
    mut task_changed_paths: BTreeMap<String, BTreeSet<String>>,
) {
    for (root, state) in root_states {
        document.known_roots.insert(root.clone(), state);
        if let Some(paths) = task_changed_paths.remove(&root) {
            document.task_changed_paths.insert(root, paths);
        } else {
            document.task_changed_paths.remove(&root);
        }
    }
}

fn closure_fingerprint(
    document: &TaskEvidenceDocument,
    frozen_diff_hash: &str,
    missing_requirement_ids: &BTreeSet<String>,
    validation_receipt_hashes: &BTreeSet<String>,
) -> String {
    canonical_hash(&serde_json::json!({
        "mutation_revision": document.mutation_revision,
        "accepted_evidence_revision": document.accepted_evidence_revision,
        "frozen_diff_hash": frozen_diff_hash,
        "missing_requirement_ids": missing_requirement_ids,
        "validation_receipt_hashes": validation_receipt_hashes,
        "review_finding_hashes": document.review_findings.iter().map(|finding| sha1_hex(finding.as_bytes())).collect::<BTreeSet<_>>(),
    }))
}

fn path_has_persistence_owner(path: &str) -> bool {
    normalize_slashes(path)
        .to_ascii_lowercase()
        .split('/')
        .any(|component| {
            matches!(
                component,
                "state"
                    | "thread-store"
                    | "message-history"
                    | "rollout"
                    | "memories"
                    | "goals"
                    | "agent-graph-store"
                    | "agent-task-store"
            )
        })
}

fn review_required(document: &TaskEvidenceDocument) -> bool {
    let Some(classification) = document.classification.as_ref() else {
        return false;
    };
    let broad_change = document
        .task_changed_paths
        .values()
        .map(BTreeSet::len)
        .sum::<usize>()
        > 20;
    let changed_paths_require_review = broad_change
        || document.task_changed_paths.values().any(|paths| {
            paths.iter().any(|path| {
                let path = path.to_ascii_lowercase();
                path == "."
                    || path_has_persistence_owner(&path)
                    || path.contains("protocol")
                    || path.contains("schema")
                    || path.contains("task_evidence")
                    || path.contains("session/turn")
                    || path.contains("session/mod")
                    || path.contains("session/session")
                    || path.contains("session/handlers")
                    || path.contains("stream_events")
                    || path.contains("tasks/review")
                    || path.contains("agent/control")
                    || path.contains("agent_jobs")
                    || path.contains("delegate")
                    || path.contains("tools/registry")
                    || path.contains("tools/router")
                    || path.contains("tools/spec_plan")
                    || path.contains("handlers/mcp")
                    || path.contains("handlers/task_state")
                    || path.contains("exec_policy")
                    || path.contains("code_mode")
                    || path.contains("sandbox")
                    || path.contains("approval")
                    || path.contains("permission")
                    || path.contains("shell")
                    || path.contains("unified_exec")
                    || path.contains("exec_command")
                    || path == "core/src/exec.rs"
                    || path.ends_with("/core/src/exec.rs")
                    || path == "core/src/spawn.rs"
                    || path.ends_with("/core/src/spawn.rs")
                    || path.starts_with("utils/pty/")
                    || path.contains("/utils/pty/")
                    || path.contains("apply_patch")
                    || path.contains("rollout")
                    || path.contains("history")
                    || path.contains("recorder")
                    || path.contains("publish")
                    || path.contains("install")
                    || path.contains("persistence")
            })
        });
    classification.exhaustive
        || changed_paths_require_review
        || classification.risk_domains.iter().any(|risk| {
            matches!(
                risk.as_str(),
                "broad"
                    | "high_risk"
                    | "protocol"
                    | "schema"
                    | "execution_safety"
                    | "persistence"
                    | "lifecycle"
                    | "publish"
                    | "installation"
                    | "uncertain_wiring"
            )
        })
}

fn set_ready(document: &mut TaskEvidenceDocument, outcome: TaskOutcome) {
    document.phase = TaskPhase::Ready;
    document.outcome = Some(outcome);
    document.final_emission_committed = false;
}

fn accepted_closure_binding_is_current(document: &TaskEvidenceDocument) -> bool {
    document.accepted_closure.as_ref().is_some_and(|closure| {
        closure.task_generation == document.task_generation
            && closure.task_contract_hash == sha1_hex(document.task_contract.as_bytes())
            && closure.mutation_revision == document.mutation_revision
            && closure.accepted_evidence_revision == document.accepted_evidence_revision
            && closure.frozen_diff_hash == frozen_mutation_state_hash(document)
            && closure.terminal_outcome.is_some()
            && accepted_closure_proofs_are_present(document, closure)
    })
}

fn accepted_closure_proofs_are_present(
    document: &TaskEvidenceDocument,
    closure: &AcceptedClosure,
) -> bool {
    let validation_hashes = document
        .validation_receipts
        .iter()
        .filter(|receipt| validation_receipt_is_accepted(receipt, document.evidence_epoch))
        .map(canonical_validation_receipt_hash)
        .collect::<BTreeSet<_>>();
    let runtime_hashes = document
        .command_receipts
        .iter()
        .filter(|receipt| {
            receipt.epoch == document.evidence_epoch
                && receipt.exit_code == 0
                && !receipt.timed_out
                && command_receipt_is_runtime_evidence(receipt, document)
        })
        .map(canonical_command_receipt_hash)
        .collect::<BTreeSet<_>>();
    closure
        .validation_receipt_hashes
        .is_subset(&validation_hashes)
        && closure.runtime_evidence_hashes.is_subset(&runtime_hashes)
}

fn accepted_closure_is_current(document: &TaskEvidenceDocument) -> bool {
    accepted_closure_binding_is_current(document)
        && document
            .accepted_closure
            .as_ref()
            .is_some_and(|closure| !closure.review_required || document.clean_review_hash.is_some())
}

fn ready_state_authorized(document: &TaskEvidenceDocument) -> bool {
    document.phase == TaskPhase::Ready
        && document.outcome.is_some()
        && document.classification.is_some()
        && document
            .accepted_closure
            .as_ref()
            .is_some_and(|closure| closure.terminal_outcome == document.outcome)
        && accepted_closure_is_current(document)
}

fn exact_final_items_emitted(document: &TaskEvidenceDocument, turn_id: &str) -> bool {
    if document.active_turn_id.as_deref() != Some(turn_id) {
        return false;
    }
    let state_authorized = if document.classification.is_some() {
        ready_state_authorized(document)
    } else {
        document.mutation_revision == 0
            && document.unsupported_mutation_targets.is_empty()
            && document.pass_prohibited_mutation_targets.is_empty()
            && !matches!(
                document.phase,
                TaskPhase::Investigating | TaskPhase::Closing | TaskPhase::Reviewing
            )
    };
    state_authorized
        && document.committed_final.as_ref().is_some_and(|committed| {
            committed.task_generation == document.task_generation
                && committed.turn_id == turn_id
                && committed.evidence_revision == document.accepted_evidence_revision
                && document.pending_finals.iter().any(|pending| {
                    pending.task_generation == committed.task_generation
                        && pending.turn_id == committed.turn_id
                        && pending.item_id == committed.item_id
                        && pending.externally_emitted
                        && pending.externally_completed
                        && !pending.superseded
                })
        })
}

fn ready_outcome_completion_gate(
    document: &TaskEvidenceDocument,
    evidence_path: Option<&Path>,
) -> Option<TaskCompletionGate> {
    let outcome = document
        .outcome
        .filter(|_| document.phase == TaskPhase::Ready)?;
    let status = match outcome {
        TaskOutcome::Passed => TaskCompletionStatus::Passed,
        TaskOutcome::Partial => TaskCompletionStatus::Partial,
        TaskOutcome::Blocked => TaskCompletionStatus::Blocked,
    };
    Some(TaskCompletionGate {
        status,
        reasons: if outcome == TaskOutcome::Passed {
            Vec::new()
        } else {
            vec!["runtime correctness-closure terminated with a non-passing outcome".to_string()]
        },
        evidence_path: evidence_path.map(|path| path.to_string_lossy().into_owned()),
    })
}

fn fallback_final_terminal_event(
    turn_id: &str,
    items: &[TurnItem],
    completion: Option<TaskCompletionGate>,
) -> EventMsg {
    let last_agent_message = items.iter().rev().find_map(|item| {
        let TurnItem::AgentMessage(message) = item else {
            return None;
        };
        Some(
            message
                .content
                .iter()
                .map(|content| match content {
                    AgentMessageContent::Text { text } => text.as_str(),
                })
                .collect::<String>(),
        )
    });
    EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_string(),
        last_agent_message,
        completion,
        completed_at: Some(Utc::now().timestamp()),
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: None,
    })
}

fn serialized_values_equal<T: Serialize>(left: &T, right: &T) -> bool {
    serde_json::to_value(left).ok() == serde_json::to_value(right).ok()
}

fn final_emission_key(
    task_generation: u64,
    turn_id: &str,
    item_id: &str,
    evidence_revision: u64,
    items: &[TurnItem],
) -> String {
    canonical_hash(&serde_json::json!({
        "task_generation": task_generation,
        "turn_id": turn_id,
        "item_id": item_id,
        "evidence_revision": evidence_revision,
        "items": items,
    }))
}

fn hydrate_pending_final_emission(
    pending: &mut PendingFinalGate,
    task_generation: u64,
    evidence_revision: u64,
) {
    if pending.superseded {
        pending.emission_reserved = false;
        pending.emission_key.clear();
        pending.emission_items.clear();
        return;
    }
    if pending.emission_items.is_empty()
        && let Some(item) = pending
            .response_item
            .as_ref()
            .and_then(crate::event_mapping::parse_turn_item)
    {
        pending.emission_items.push(item);
    }
    if pending.emission_key.is_empty() && !pending.emission_items.is_empty() {
        pending.emission_key = final_emission_key(
            task_generation,
            &pending.turn_id,
            &pending.item_id,
            evidence_revision,
            &pending.emission_items,
        );
    }
}

fn supersede_pending_final(pending: &mut PendingFinalGate) -> bool {
    let changed = pending.emission_reserved
        || !pending.superseded
        || !pending.emission_key.is_empty()
        || !pending.emission_items.is_empty();
    pending.emission_reserved = false;
    pending.superseded = true;
    pending.emission_key.clear();
    pending.emission_items.clear();
    changed
}

fn upsert_pending_final(document: &mut TaskEvidenceDocument, pending: PendingFinalGate) {
    if let Some(existing) = document
        .pending_finals
        .iter_mut()
        .find(|existing| existing.turn_id == pending.turn_id && existing.item_id == pending.item_id)
    {
        existing.task_generation = pending.task_generation;
        existing.evidence_revision = pending.evidence_revision;
        existing.persisted |= pending.persisted;
        existing.history_position = pending.history_position.or(existing.history_position);
        existing.history_compacted |= pending.history_compacted;
        existing.emission_reserved = pending.emission_reserved;
        existing.externally_emitted |= pending.externally_emitted;
        existing.externally_completed |= pending.externally_completed;
        existing.superseded = pending.superseded;
        if pending.response_item.is_some() {
            existing.response_item = pending.response_item;
        }
        if !pending.emission_key.is_empty() {
            existing.emission_key = pending.emission_key;
        }
        if !pending.emission_items.is_empty() {
            existing.emission_items = pending.emission_items;
        }
    } else {
        document.pending_finals.push(pending);
    }
}

fn tool_is_normalized_shell(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell_command" | "exec_command" | "write_stdin" | "exec" | "wait"
    )
}

fn tool_is_lifecycle_or_read_only(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "task_state"
            | "update_plan"
            | "search_source"
            | "read_file_span"
            | "view_image"
            | "list_mcp_resources"
            | "list_mcp_resource_templates"
            | "read_mcp_resource"
            | "tool_search"
            | "get_context_remaining"
            | "current_time"
            | "sleep"
            | "wait_for_environment"
            | "list_agents"
            | "get_agent_task"
            | "wait_agent"
            | "request_user_input"
            | "imagegen"
            | "web_search"
            | "get_goal"
            | "inspect_goal"
    )
}

fn tool_is_supported_local_mutator(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "normalized_shell_mutation"
            | "apply_patch"
            | "verify_local"
            | "spawn_agent"
            | "followup_task"
            | "spawn_agents_on_csv"
            | "amend_agent_task"
            | "abandon_agent_task"
            | "set_agent_gate"
            | "waive_agent_gate"
            | "submit_agent_receipt"
    )
}

fn command_is_proven_read_only(command: &[String]) -> bool {
    codex_shell_command::is_safe_command::is_known_safe_command(command)
}

struct CommandMutationTargets {
    paths: Vec<PathBuf>,
    discovery_complete: bool,
}

fn command_mutation_targets(command: &[String], cwd: &Path) -> CommandMutationTargets {
    let mut targets = BTreeSet::new();
    let mut tokens = Vec::new();
    for part in command.iter().skip(1) {
        tokens.extend(
            part.split(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '`' | ';' | '|' | ',' | '(' | ')' | '{' | '}'
                    )
            })
            .filter(|token| !token.is_empty())
            .map(str::to_string),
        );
    }
    for (index, token) in tokens.iter().enumerate() {
        let candidate = token
            .split_once('=')
            .map_or(token.as_str(), |(_, value)| value)
            .trim_matches(|character: char| matches!(character, '[' | ']' | ':'));
        let path = Path::new(candidate);
        let has_parent_traversal = path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir));
        let follows_directory_change = index > 0
            && matches!(
                tokens[index - 1].to_ascii_lowercase().as_str(),
                "cd" | "chdir" | "set-location" | "push-location"
            );
        let follows_path_bearing_token = index > 0
            && matches!(
                tokens[index - 1].to_ascii_lowercase().as_str(),
                "touch"
                    | "mkdir"
                    | "md"
                    | "new-item"
                    | "set-content"
                    | "add-content"
                    | "out-file"
                    | "remove-item"
                    | "rm"
                    | "del"
                    | "copy-item"
                    | "move-item"
                    | "cp"
                    | "mv"
                    | "tee"
                    | "-c"
                    | "--directory"
                    | "-path"
                    | "-literalpath"
                    | "-destination"
            );
        let slash_style_option = candidate
            .strip_prefix('/')
            .is_some_and(|suffix| !suffix.contains(['/', '\\']));
        let meaningful_relative_operand = !path.is_absolute()
            && !candidate.starts_with('-')
            && !slash_style_option
            && !candidate.contains("://")
            && (has_parent_traversal
                || follows_directory_change
                || follows_path_bearing_token
                || candidate.contains(['/', '\\'])
                || cwd.join(path).exists());
        if path.is_absolute() || meaningful_relative_operand {
            targets.insert(if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            });
        }
    }
    let has_dynamic_target_syntax = command.iter().any(|part| {
        part.contains('$')
            || part.contains('%')
            || part.contains('*')
            || part.contains('?')
            || part.contains('`')
    });
    let executable = command
        .first()
        .and_then(|part| part.rsplit(['/', '\\']).next())
        .unwrap_or_default()
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    let static_target_owner = matches!(
        executable.as_str(),
        "git"
            | "touch"
            | "mkdir"
            | "md"
            | "rm"
            | "del"
            | "cp"
            | "mv"
            | "new-item"
            | "set-content"
            | "add-content"
            | "out-file"
            | "remove-item"
            | "copy-item"
            | "move-item"
    ) || (matches!(
        executable.as_str(),
        "powershell" | "pwsh" | "bash" | "sh" | "zsh" | "cmd"
    ) && tokens.iter().any(|token| {
        matches!(
            token.to_ascii_lowercase().as_str(),
            "git"
                | "touch"
                | "mkdir"
                | "md"
                | "rm"
                | "del"
                | "cp"
                | "mv"
                | "new-item"
                | "set-content"
                | "add-content"
                | "out-file"
                | "remove-item"
                | "copy-item"
                | "move-item"
        )
    }));
    let discovery_complete = !has_dynamic_target_syntax && static_target_owner;
    CommandMutationTargets {
        paths: targets.into_iter().collect(),
        discovery_complete,
    }
}

fn ensure_mutation_phase(document: &TaskEvidenceDocument, turn_id: &str) -> Result<(), String> {
    if document.active_turn_id.as_deref() != Some(turn_id) {
        return Err(
            "mutation rejected because its turn is no longer the active task turn".to_string(),
        );
    }
    match document.phase {
        TaskPhase::Unclassified if document.classification.is_none() => Err(
            "mutation rejected: call task_state.classify before using mutating tools".to_string(),
        ),
        _ if document.classification.is_none() => {
            Err("mutation rejected: task state has no accepted classification".to_string())
        }
        TaskPhase::Investigating | TaskPhase::Closing | TaskPhase::Reviewing => Err(format!(
            "mutation rejected while task phase is {:?}",
            document.phase
        )),
        TaskPhase::Fixing
            if document
                .classification
                .as_ref()
                .is_some_and(|classification| classification.exhaustive)
                && document.investigation_checkpoint_hash.is_none() =>
        {
            Err(
                "mutation rejected: exhaustive work requires an accepted investigation checkpoint"
                    .to_string(),
            )
        }
        TaskPhase::Unclassified => {
            Err("mutation rejected: classified task has an invalid Unclassified phase".to_string())
        }
        TaskPhase::Ready if document.final_emission_committed => Err(
            "mutation rejected: final output already committed; start a new task turn".to_string(),
        ),
        TaskPhase::Ready | TaskPhase::Fixing => Ok(()),
    }
}

fn invalidate_for_mutation(document: &mut TaskEvidenceDocument) {
    document.mutation_revision = document.mutation_revision.saturating_add(1);
    document.phase = TaskPhase::Fixing;
    document.outcome = None;
    document.accepted_closure = None;
    document.clean_review_hash = None;
    document.prepared_review = None;
    document.latest_review_finding_revision = None;
    document.latest_actionable_finding_revision = None;
    document.closure_fingerprint = None;
    document.final_emission_committed = false;
    document.committed_final = None;
    invalidate_final_reservations(document);
    invalidate_evidence(document, true, true);
}

fn invalidate_for_scope_change(document: &mut TaskEvidenceDocument) {
    if document
        .classification
        .as_ref()
        .is_some_and(|classification| classification.exhaustive)
    {
        document.investigation_checkpoint_hash = None;
    }
    invalidate_for_plan_change(document);
}

fn invalidate_for_plan_change(document: &mut TaskEvidenceDocument) {
    if document
        .classification
        .as_ref()
        .is_some_and(|classification| classification.exhaustive)
        && document.investigation_checkpoint_hash.is_none()
    {
        document.phase = TaskPhase::Investigating;
    } else if document.classification.is_some() {
        document.phase = TaskPhase::Fixing;
    }
    document.outcome = None;
    document.accepted_closure = None;
    document.clean_review_hash = None;
    document.prepared_review = None;
    document.closure_fingerprint = None;
    document.incomplete_occurrences.clear();
    document.review_attempt_failures.clear();
    document.final_emission_committed = false;
    document.committed_final = None;
    invalidate_final_reservations(document);
    invalidate_evidence(document, false, false);
}

fn invalidate_for_failed_validation(document: &mut TaskEvidenceDocument) {
    document.outcome = None;
    document.accepted_closure = None;
    document.clean_review_hash = None;
    document.prepared_review = None;
    document.closure_fingerprint = None;
    document.final_emission_committed = false;
    document.committed_final = None;
    document.phase = if document.classification.is_some() {
        TaskPhase::Fixing
    } else {
        TaskPhase::Unclassified
    };
    invalidate_final_reservations(document);
    invalidate_evidence(document, false, false);
}

fn invalidate_final_reservations(document: &mut TaskEvidenceDocument) {
    let task_generation = document.task_generation;
    for pending in &mut document.pending_finals {
        if pending.task_generation == task_generation && !pending.externally_emitted {
            supersede_pending_final(pending);
        }
    }
}

fn invalidate_evidence(
    document: &mut TaskEvidenceDocument,
    reset_repair_budget: bool,
    file_mutation: bool,
) {
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    if file_mutation {
        document.last_mutation_at = Some(timestamp());
    }
    document.verify_plan_epoch = None;
    document.validation_epoch = None;
    document.desktop_activation_receipt = None;
    document.automatic_plan_attempt_epoch = None;
    if reset_repair_budget {
        document.repair_turns_used = 0;
    }
    document.completion = None;
    for step in &mut document.plan {
        if step.status == StepStatus::Passed {
            step.status = StepStatus::Implemented;
        }
        step.validation_receipt_ids.clear();
    }
    for requirement in &mut document.generated_artifact_requirements {
        requirement.validation_receipt_ids.clear();
    }
}

fn task_is_tracked(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        || !document.edit_receipts.is_empty()
        || document
            .command_receipts
            .iter()
            .any(|receipt| receipt.possible_mutation)
        || document
            .risks
            .iter()
            .any(|risk| risk.source == "task_evidence_storage" && !risk.resolved)
}

fn resolve_risks_by_source(document: &mut TaskEvidenceDocument, source: &str) {
    for risk in &mut document.risks {
        if risk.source == source {
            risk.resolved = true;
        }
    }
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
        blocking: true,
        resolved: false,
        epoch,
    }
}

fn unreadable_file_risk_id(path: &str) -> String {
    let digest = sha1_hex(normalize_slashes(path).as_bytes());
    format!("unreadable-file-{}", &digest[..16])
}

fn generated_artifact_freshness_risk_id(path: &str) -> String {
    let digest = sha1_hex(normalize_slashes(path).as_bytes());
    format!("generated-artifact-freshness-{}", &digest[..16])
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
    match tokio::fs::read(&absolute).await {
        Ok(bytes) => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: Some(sha1_hex(&bytes)),
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

fn path_is_covered(path: &str, active: &str) -> bool {
    let path = normalize_slashes(path);
    let active = normalize_slashes(active);
    path == active || path.starts_with(&format!("{active}/"))
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
    use std::process::Command;
    use tempfile::TempDir;

    async fn ledger_fixture() -> (TempDir, TaskEvidenceLedger) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(repo.join("scripts"))
            .await
            .expect("scripts");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
            .await
            .expect("verifier");
        tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
            .await
            .expect("manifest");
        run_git(&repo, &["add", "."]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
        let ledger =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        ledger
            .begin_turn("turn", "fixture task")
            .await
            .expect("begin turn");
        (temp, ledger)
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn initialize_test_git_repo(repo: &Path, contents: &str) {
        tokio::fs::create_dir_all(repo).await.expect("repo");
        run_git(repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join("tracked.txt"), contents)
            .await
            .expect("tracked file");
        run_git(repo, &["add", "tracked.txt"]);
        run_git(
            repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
    }

    async fn assert_no_preserved_task_evidence(codex_home: &Path) {
        let mut entries = tokio::fs::read_dir(codex_home.join("task-evidence"))
            .await
            .expect("task evidence directory");
        while let Some(entry) = entries.next_entry().await.expect("task evidence entry") {
            assert!(
                !entry.file_name().to_string_lossy().contains(".preserved"),
                "repository overrides must not quarantine a valid thread ledger"
            );
        }
    }

    async fn git_ledger_fixture() -> (TempDir, PathBuf, TaskEvidenceLedger) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&repo).await.expect("repo");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join("tracked.txt"), "initial")
            .await
            .expect("tracked file");
        run_git(&repo, &["add", "tracked.txt"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
        let ledger =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        ledger
            .begin_turn("turn", "fixture task")
            .await
            .expect("begin turn");
        (temp, repo, ledger)
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
    async fn legacy_completed_is_reopened_until_fresh_evidence_exists() {
        let (_temp, ledger) = ledger_fixture().await;
        let normalized = ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;
        assert_eq!(normalized.plan[0].status, StepStatus::Implemented);
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Partial);
        assert!(
            gate.reasons
                .iter()
                .any(|reason| reason.contains("verify_local planning"))
        );
    }

    #[tokio::test]
    async fn edit_after_validation_reopens_step_and_stales_receipts() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        tokio::fs::create_dir_all(repo.join("src"))
            .await
            .expect("src");
        tokio::fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 1 }")
            .await
            .expect("source");
        ledger
            .record_plan_update(&plan(StepStatus::InProgress))
            .await;
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("repo");
        ledger
            .record_edit_intent("patch-1", cwd.as_path(), &[PathBuf::from("src/lib.rs")])
            .await;
        tokio::fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 2 }")
            .await
            .expect("source update");
        ledger.record_edit_result("patch-1", "completed").await;
        let plan_validation_start = ledger
            .begin_verify_local_validation()
            .await
            .expect("plan validation start");
        ledger
            .record_verify_local(
                "plan",
                Some("PLANNED"),
                true,
                false,
                Some(&plan_validation_start),
                &[PathBuf::from("src/lib.rs")],
                &[],
                Some(&serde_json::json!({"planned": []})),
            )
            .await;
        let final_validation_start = ledger
            .begin_verify_local_validation()
            .await
            .expect("final validation start");
        ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                Some(&final_validation_start),
                &[PathBuf::from("src/lib.rs")],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await;
        assert_eq!(
            ledger.completion_gate().await.expect("gate").status,
            TaskCompletionStatus::Passed
        );

        ledger
            .record_edit_intent("patch-2", cwd.as_path(), &[PathBuf::from("src/lib.rs")])
            .await;
        tokio::fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 3 }")
            .await
            .expect("second update");
        ledger.record_edit_result("patch-2", "completed").await;
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Partial);
        assert!(
            gate.reasons
                .iter()
                .any(|reason| reason.contains("missing or stale"))
        );
    }

    #[tokio::test]
    async fn missing_generation_and_desktop_activation_are_blocking() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut update = plan(StepStatus::Completed);
        update.plan[0].generated_artifacts = vec!["generated/missing.json".to_string()];
        update.plan[0].requires_desktop_activation = true;
        ledger.record_plan_update(&update).await;
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Blocked);
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
    }

    #[tokio::test]
    async fn finalization_warning_is_bounded_and_does_not_request_a_turn() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;
        let warning = ledger.take_finalization_warning().await.expect("warning");
        assert!(warning.contains("No automatic repair turn was started"));
        assert!(ledger.take_finalization_warning().await.is_none());
    }

    #[tokio::test]
    async fn automatic_verify_plan_is_requested_once_per_mutation_epoch() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        tokio::fs::create_dir_all(repo.join("src"))
            .await
            .expect("src");
        tokio::fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 1 }")
            .await
            .expect("source");
        ledger
            .record_plan_update(&plan(StepStatus::InProgress))
            .await;
        ledger
            .record_edit_intent("patch-1", &repo, &[PathBuf::from("src/lib.rs")])
            .await;
        tokio::fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 2 }")
            .await
            .expect("source update");
        ledger.record_edit_result("patch-1", "completed").await;

        assert_eq!(
            ledger.take_automatic_verify_plan_request().await,
            Some(vec!["src/lib.rs".to_string()])
        );
        assert_eq!(ledger.take_automatic_verify_plan_request().await, None);
    }

    fn ordinary_classification(risk_domains: &[&str]) -> TaskClassification {
        TaskClassification {
            exhaustive: false,
            risk_domains: risk_domains
                .iter()
                .map(|risk| (*risk).to_string())
                .collect(),
            supported_non_git_roots: BTreeSet::new(),
        }
    }

    fn closure_with_missing(missing: &[&str]) -> ClosureSubmission {
        ClosureSubmission {
            path_review: BTreeSet::from([".".to_string()]),
            competing_paths_checked: BTreeSet::from([".".to_string()]),
            validation_receipt_ids: BTreeSet::new(),
            runtime_evidence: BTreeSet::new(),
            missing_requirement_ids: missing
                .iter()
                .map(|requirement| (*requirement).to_string())
                .collect(),
            actionable_findings: BTreeSet::new(),
            blocked_reasons: BTreeSet::new(),
        }
    }

    fn review_receipt(findings: BTreeSet<String>, verdict: &str) -> TaskReviewReceipt {
        TaskReviewReceipt {
            findings,
            verdict: verdict.to_string(),
            explanation: "reviewed the frozen patch".to_string(),
            confidence_score_millis: 1000,
        }
    }

    fn force_ready_for_test(document: &mut TaskEvidenceDocument, outcome: TaskOutcome) {
        document.accepted_closure = Some(AcceptedClosure {
            task_generation: document.task_generation,
            task_contract_hash: sha1_hex(document.task_contract.as_bytes()),
            receipt_hash: "test-closure".to_string(),
            mutation_revision: document.mutation_revision,
            accepted_evidence_revision: document.accepted_evidence_revision,
            frozen_diff_hash: frozen_mutation_state_hash(document),
            terminal_outcome: Some(outcome),
            missing_requirement_ids: BTreeSet::new(),
            validation_receipt_hashes: BTreeSet::new(),
            runtime_evidence_hashes: BTreeSet::new(),
            review_required: false,
        });
        set_ready(document, outcome);
    }

    async fn reserve_persisted_final_for_test(
        ledger: &TaskEvidenceLedger,
        item_id: &str,
        text: &str,
    ) {
        assert!(
            ledger
                .authorize_final_item("turn", item_id)
                .await
                .expect("final reservation")
        );
        ledger
            .mark_final_item_persisted(
                "turn",
                item_id,
                &codex_protocol::models::ResponseItem::Message {
                    id: Some(item_id.to_string()),
                    role: "assistant".to_string(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: text.to_string(),
                    }],
                    phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await
            .expect("persist final");
    }

    #[tokio::test]
    async fn accepted_steering_extends_contract_and_invalidates_ready_closure() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");

        ledger
            .extend_task_contract("turn", "also preserve the new runtime constraint")
            .await
            .expect("extend task contract");

        let document = ledger.document.lock().await;
        let document = document.as_ref().expect("document");
        assert_eq!(
            document.task_contract,
            "fixture task\nalso preserve the new runtime constraint"
        );
        assert_eq!(document.phase, TaskPhase::Fixing);
        assert_eq!(document.outcome, None);
        assert!(document.accepted_closure.is_none());
    }

    #[tokio::test]
    async fn exhaustive_classification_requires_one_checkpoint() {
        let (_temp, ledger) = ledger_fixture().await;
        let status = ledger
            .classify(TaskClassification {
                exhaustive: true,
                ..ordinary_classification(&[])
            })
            .await
            .expect("classification");
        assert_eq!(status.phase, TaskPhase::Investigating);
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("apply_patch"),
                    "turn",
                    false,
                    false,
                    false,
                    false,
                    true,
                )
                .await
                .is_err()
        );

        let status = ledger
            .submit_investigation_checkpoint(InvestigationCheckpoint {
                summary: "mapped the complete path".to_string(),
                paths_reviewed: BTreeSet::from(["scripts/verify_local.py".to_string()]),
                competing_paths_checked: BTreeSet::from(["kd4_features.toml".to_string()]),
            })
            .await
            .expect("checkpoint");
        assert_eq!(status.phase, TaskPhase::Fixing);
        assert_eq!(status.accepted_evidence_revision, 1);
    }

    #[tokio::test]
    async fn exhaustive_contract_extension_requires_a_fresh_investigation_checkpoint() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(TaskClassification {
                exhaustive: true,
                ..ordinary_classification(&[])
            })
            .await
            .expect("classification");
        ledger
            .submit_investigation_checkpoint(InvestigationCheckpoint {
                summary: "mapped the original scope".to_string(),
                paths_reviewed: BTreeSet::from(["scripts/verify_local.py".to_string()]),
                competing_paths_checked: BTreeSet::from(["kd4_features.toml".to_string()]),
            })
            .await
            .expect("checkpoint");

        ledger
            .extend_task_contract("turn", "also audit the persistence path")
            .await
            .expect("extend contract");
        let status = ledger.inspect_status().await.expect("status");
        assert_eq!(status.phase, TaskPhase::Investigating);
        assert!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .investigation_checkpoint_hash
                .is_none()
        );
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("apply_patch"),
                    "turn",
                    false,
                    false,
                    false,
                    false,
                    true,
                )
                .await
                .is_err()
        );

        let status = ledger
            .submit_investigation_checkpoint(InvestigationCheckpoint {
                summary: "mapped the extended scope".to_string(),
                paths_reviewed: BTreeSet::from(["scripts/verify_local.py".to_string()]),
                competing_paths_checked: BTreeSet::from(["kd4_features.toml".to_string()]),
            })
            .await
            .expect("fresh checkpoint");
        assert_eq!(status.phase, TaskPhase::Fixing);
    }

    #[tokio::test]
    async fn exhaustive_plan_update_preserves_investigation_until_checkpoint() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(TaskClassification {
                exhaustive: true,
                ..ordinary_classification(&[])
            })
            .await
            .expect("classification");

        ledger
            .try_record_plan_update(&plan(StepStatus::InProgress))
            .await
            .expect("material plan update");
        assert_eq!(
            ledger.inspect_status().await.expect("status").phase,
            TaskPhase::Investigating
        );

        ledger
            .submit_investigation_checkpoint(InvestigationCheckpoint {
                summary: "mapped the complete path after planning".to_string(),
                paths_reviewed: BTreeSet::from(["scripts/verify_local.py".to_string()]),
                competing_paths_checked: BTreeSet::from(["kd4_features.toml".to_string()]),
            })
            .await
            .expect("checkpoint after plan update");
        let mutation_guard = ledger
            .guard_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("mutation dispatch after checkpoint")
            .expect("managed mutation guard");
        drop(mutation_guard);
    }

    #[tokio::test]
    async fn exhaustive_checkpoint_rejects_blank_missing_and_outside_paths() {
        let (temp, ledger) = ledger_fixture().await;
        ledger
            .classify(TaskClassification {
                exhaustive: true,
                ..ordinary_classification(&[])
            })
            .await
            .expect("classification");
        for paths_reviewed in [
            BTreeSet::from(["".to_string()]),
            BTreeSet::from(["missing.rs".to_string()]),
            BTreeSet::from([temp.path().join("outside").to_string_lossy().into_owned()]),
        ] {
            assert!(
                ledger
                    .submit_investigation_checkpoint(InvestigationCheckpoint {
                        summary: "reviewed the relevant path".to_string(),
                        paths_reviewed,
                        competing_paths_checked: BTreeSet::new(),
                    })
                    .await
                    .is_err()
            );
            assert_eq!(
                ledger.inspect_status().await.expect("status").phase,
                TaskPhase::Investigating
            );
        }
    }

    #[tokio::test]
    async fn mutation_dispatch_requires_classification_even_when_tool_is_hidden() {
        let (_temp, ledger) = ledger_fixture().await;
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("apply_patch"),
                    "turn",
                    false,
                    false,
                    false,
                    false,
                    true,
                )
                .await
                .is_err()
        );
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("hidden_read_only_tool"),
                    "turn",
                    true,
                    false,
                    false,
                    false,
                    true,
                )
                .await
                .is_ok()
        );
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("search_source"),
                    "turn",
                    true,
                    false,
                    false,
                    false,
                    false,
                )
                .await
                .is_err(),
            "an external handler cannot spoof a built-in read-only name or hint"
        );
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("update_plan"),
                    "turn",
                    false,
                    false,
                    true,
                    false,
                    true,
                )
                .await
                .is_err()
        );
        let nested_root = ledger.repo_root.as_ref().expect("root").join("nested");
        tokio::fs::create_dir_all(nested_root.join(".git"))
            .await
            .expect("nested git marker");
        let mutating_command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Set-Content result.txt changed".to_string(),
        ];
        assert!(
            ledger
                .guard_normalized_command(
                    &mutating_command,
                    Some(&nested_root),
                    "turn",
                    false,
                    false,
                )
                .await
                .is_err()
        );
        let status = ledger.inspect_status().await.expect("status");
        assert_eq!(status.phase, TaskPhase::Unclassified);
        assert_eq!(status.mutation_revision, 0);

        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        assert!(
            ledger
                .guard_tool_dispatch(
                    &ToolName::plain("apply_patch"),
                    "turn",
                    false,
                    false,
                    false,
                    false,
                    true,
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn successful_validation_dispatch_does_not_manufacture_a_mutation_revision() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let before = ledger.inspect_status().await.expect("status");
        let mutation_guard = ledger
            .reserve_tool_dispatch(
                &ToolName::plain("verify_local"),
                "turn",
                /*declared_read_only*/ false,
                /*trusted_external_read_only*/ false,
                /*force_read_only*/ false,
                /*trusted_mutator*/ false,
                /*trusted_builtin*/ true,
            )
            .await
            .expect("reserve validation dispatch")
            .expect("validation execution should hold the mutation fence");

        assert!(
            !ledger
                .finish_reserved_tool_dispatch("turn", &mutation_guard)
                .await
                .expect("finish validation dispatch")
        );
        drop(mutation_guard);
        let after = ledger.inspect_status().await.expect("status");
        assert_eq!(after.mutation_revision, before.mutation_revision);
        assert_eq!(
            after.accepted_evidence_revision,
            before.accepted_evidence_revision
        );
    }

    #[tokio::test]
    async fn validation_dispatch_still_records_real_repository_drift_as_mutation() {
        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let before = ledger.inspect_status().await.expect("status");
        let mutation_guard = ledger
            .reserve_tool_dispatch(
                &ToolName::plain("verify_local"),
                "turn",
                /*declared_read_only*/ false,
                /*trusted_external_read_only*/ false,
                /*force_read_only*/ false,
                /*trusted_mutator*/ false,
                /*trusted_builtin*/ true,
            )
            .await
            .expect("reserve validation dispatch")
            .expect("validation execution should hold the mutation fence");
        tokio::fs::write(repo.join("tracked.txt"), "changed during validation")
            .await
            .expect("mutate tracked fixture");

        assert!(
            ledger
                .finish_reserved_tool_dispatch("turn", &mutation_guard)
                .await
                .expect("finish validation dispatch")
        );
        drop(mutation_guard);
        let after = ledger.inspect_status().await.expect("status");
        assert_eq!(after.mutation_revision, before.mutation_revision + 1);
        assert_eq!(after.phase, TaskPhase::Fixing);
    }

    #[tokio::test]
    async fn external_read_only_dispatch_requires_trusted_router_provenance() {
        let (_temp, ledger) = ledger_fixture().await;
        let external_reader = ToolName::namespaced("mcp__reader", "lookup");

        ledger
            .guard_tool_dispatch(
                &external_reader,
                "turn",
                /*declared_read_only*/ true,
                /*trusted_external_read_only*/ true,
                /*force_read_only*/ false,
                /*trusted_mutator*/ false,
                /*trusted_builtin*/ false,
            )
            .await
            .expect("router-proven reader should be allowed while unclassified");
        assert!(
            ledger
                .guard_tool_dispatch(
                    &external_reader,
                    "turn",
                    /*declared_read_only*/ true,
                    /*trusted_external_read_only*/ false,
                    /*force_read_only*/ false,
                    /*trusted_mutator*/ false,
                    /*trusted_builtin*/ false,
                )
                .await
                .is_err(),
            "a handler declaration without trusted router provenance must be rejected"
        );
        assert!(
            ledger
                .guard_tool_dispatch(
                    &external_reader,
                    "turn",
                    /*declared_read_only*/ false,
                    /*trusted_external_read_only*/ false,
                    /*force_read_only*/ false,
                    /*trusted_mutator*/ false,
                    /*trusted_builtin*/ false,
                )
                .await
                .is_err(),
            "false or missing read-only hints must be rejected"
        );

        let status = ledger
            .classify(TaskClassification {
                exhaustive: true,
                risk_domains: BTreeSet::new(),
                supported_non_git_roots: BTreeSet::new(),
            })
            .await
            .expect("classification");
        assert_eq!(status.phase, TaskPhase::Investigating);
        ledger
            .guard_tool_dispatch(
                &external_reader,
                "turn",
                /*declared_read_only*/ true,
                /*trusted_external_read_only*/ true,
                /*force_read_only*/ false,
                /*trusted_mutator*/ false,
                /*trusted_builtin*/ false,
            )
            .await
            .expect("router-proven reader should be allowed while investigating");
        assert!(
            ledger
                .guard_tool_dispatch(
                    &external_reader,
                    "turn",
                    /*declared_read_only*/ true,
                    /*trusted_external_read_only*/ true,
                    /*force_read_only*/ true,
                    /*trusted_mutator*/ false,
                    /*trusted_builtin*/ false,
                )
                .await
                .is_err(),
            "independent reviewers must remain isolated from external tools"
        );
    }

    #[tokio::test]
    async fn normalized_detach_syntax_uses_runtime_containment_instead_of_lexical_rejection() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let repo = ledger.repo_root.as_ref().expect("root");
        let command = vec![
            "sh".to_string(),
            "-lc".to_string(),
            "nohup helper &".to_string(),
        ];
        let guard = ledger
            .guard_normalized_command(&command, Some(repo), "turn", false, false)
            .await
            .expect("runtime containment, not command spelling, owns descendant safety");
        assert!(guard.is_some());
    }

    #[test]
    fn nearest_git_root_resolution_supports_nested_repositories_and_worktrees() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outer = temp.path().join("outer");
        let nested = outer.join("nested");
        let nested_child = nested.join("src");
        std::fs::create_dir_all(outer.join(".git")).expect("outer git marker");
        std::fs::create_dir_all(&nested_child).expect("nested child");
        std::fs::write(nested.join(".git"), "gitdir: ../worktrees/nested")
            .expect("worktree marker");

        assert_eq!(find_git_repo_root(&nested_child), Some(nested));
        assert_eq!(find_git_repo_root(&outer), Some(outer));
    }

    #[tokio::test]
    async fn supported_non_git_mutation_is_never_eligible_for_passed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("non-git");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("non-git root");
        let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), &root).await;
        ledger
            .classify(TaskClassification {
                supported_non_git_roots: BTreeSet::from([root.to_string_lossy().into_owned()]),
                ..ordinary_classification(&[])
            })
            .await
            .expect("classification");

        ledger
            .register_mutation_targets(&root, &[root.join("result.txt")])
            .await
            .expect("explicitly supported non-git mutation");
        let status = ledger.inspect_status().await.expect("status");
        assert!(status.unsupported_mutation_targets.is_empty());
        assert_eq!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .pass_prohibited_mutation_targets
                .len(),
            1
        );

        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let first = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        let second = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("second closure");
        assert_eq!(second.phase, TaskPhase::Ready);
        assert_eq!(second.outcome, Some(TaskOutcome::Blocked));
    }

    #[tokio::test]
    async fn unsupported_target_discovery_invalidates_ready_before_handler_execution() {
        let (temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let outside = temp.path().join("outside");
        tokio::fs::create_dir_all(&outside)
            .await
            .expect("outside directory");

        ledger
            .register_mutation_targets(&repo, &[outside.join("result.txt")])
            .await
            .expect_err("unsupported owner must be rejected");

        let document = ledger.document.lock().await;
        let document = document.as_ref().expect("document");
        assert_eq!(document.phase, TaskPhase::Fixing);
        assert_eq!(document.outcome, None);
        assert!(document.accepted_closure.is_none());
        assert!(!document.unsupported_mutation_targets.is_empty());
    }

    #[tokio::test]
    async fn newly_observed_root_invalidates_ready_before_handler_execution() {
        let (temp, repo, ledger) = git_ledger_fixture().await;
        let nested = temp.path().join("nested");
        tokio::fs::create_dir_all(&nested)
            .await
            .expect("nested root");
        run_git(&nested, &["init", "--quiet"]);
        tokio::fs::write(nested.join("tracked.txt"), "nested")
            .await
            .expect("nested file");
        run_git(&nested, &["add", "tracked.txt"]);
        run_git(
            &nested,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "nested fixture",
            ],
        );
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let before = ledger
            .inspect_status()
            .await
            .expect("status")
            .mutation_revision;

        ledger
            .register_mutation_targets(&repo, &[nested.join("tracked.txt")])
            .await
            .expect("register nested root");

        let status = ledger.inspect_status().await.expect("status");
        assert_eq!(status.phase, TaskPhase::Fixing);
        assert_eq!(status.outcome, None);
        assert_eq!(status.mutation_revision, before + 1);
        let canonical_nested = dunce::canonicalize(nested)
            .expect("canonical nested")
            .to_string_lossy()
            .into_owned();
        assert!(status.known_roots.contains(&canonical_nested));
    }

    #[tokio::test]
    async fn relative_shell_operand_registers_ignored_nested_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        let nested = repo.join("nested");
        tokio::fs::create_dir_all(&repo)
            .await
            .expect("outer repository");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join(".gitignore"), "nested/\n")
            .await
            .expect("ignore nested repository");
        tokio::fs::write(repo.join("outer.txt"), "initial")
            .await
            .expect("outer tracked file");
        run_git(&repo, &["add", ".gitignore", "outer.txt"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "outer fixture",
            ],
        );
        tokio::fs::create_dir_all(&nested)
            .await
            .expect("nested repository");
        run_git(&nested, &["init", "--quiet"]);
        tokio::fs::write(nested.join("inner.txt"), "initial")
            .await
            .expect("nested tracked file");
        run_git(&nested, &["add", "inner.txt"]);
        run_git(
            &nested,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "nested fixture",
            ],
        );
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute outer repo");
        let ledger =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        ledger
            .begin_turn("turn", "mutate the ignored nested repository")
            .await
            .expect("begin turn");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");

        let mutation_guard = ledger
            .guard_normalized_command(
                &[
                    "powershell".to_string(),
                    "-Command".to_string(),
                    "Set-Content nested/inner.txt changed".to_string(),
                ],
                Some(&repo),
                "turn",
                false,
                false,
            )
            .await
            .expect("relative nested mutation should be authorized");
        assert!(mutation_guard.is_some());
        drop(mutation_guard);

        let canonical_nested = dunce::canonicalize(&nested)
            .expect("canonical nested root")
            .to_string_lossy()
            .into_owned();
        let status = ledger.inspect_status().await.expect("status");
        assert!(
            status.known_roots.contains(&canonical_nested),
            "relative shell target should register its nearest nested Git root"
        );

        tokio::fs::write(nested.join("inner.txt"), "changed")
            .await
            .expect("mutate nested tracked file");
        let drifted = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("closure drift check");
        assert_eq!(drifted.phase, TaskPhase::Fixing);
        assert!(drifted.mutation_revision > status.mutation_revision);
        assert!(drifted.message.contains("repository drift"));
    }

    #[tokio::test]
    async fn rejected_shell_ownership_does_not_advance_mutation_revision() {
        let (temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let repo = temp.path().join("repo");
        tokio::fs::create_dir_all(temp.path().join("outside"))
            .await
            .expect("outside directory");
        let command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Set-Content ../outside/result.txt changed".to_string(),
        ];
        let before = ledger.inspect_status().await.expect("status");

        for _ in 0..2 {
            let error = ledger
                .guard_normalized_command(&command, Some(&repo), "turn", false, false)
                .await
                .expect_err("unsupported non-Git ownership must be rejected");
            assert!(error.contains("not owned by a registered Git root"));
            let after = ledger.inspect_status().await.expect("status");
            assert_eq!(after.mutation_revision, before.mutation_revision);
            assert_eq!(
                after.accepted_evidence_revision,
                before.accepted_evidence_revision
            );
        }
    }

    #[tokio::test]
    async fn deferred_tool_rejection_does_not_manufacture_mutation_progress() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let before = ledger.inspect_status().await.expect("status");

        let reservation = ledger
            .reserve_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("dispatch reservation")
            .expect("managed mutation reservation");
        drop(reservation);

        let rejected = ledger.inspect_status().await.expect("status");
        assert_eq!(rejected.mutation_revision, before.mutation_revision);
        assert_eq!(
            rejected.accepted_evidence_revision,
            before.accepted_evidence_revision
        );
        assert_eq!(rejected.closure_fingerprint, before.closure_fingerprint);

        let reservation = ledger
            .reserve_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("dispatch reservation")
            .expect("managed mutation reservation");
        ledger
            .start_reserved_tool_dispatch("turn", &reservation)
            .await
            .expect("start mutation before handler");
        let started = ledger.inspect_status().await.expect("started status");
        assert_eq!(started.mutation_revision, before.mutation_revision + 1);
        assert!(
            ledger
                .finish_reserved_tool_dispatch("turn", &reservation)
                .await
                .expect("successful mutation commit")
        );
        drop(reservation);
        assert_eq!(
            ledger
                .inspect_status()
                .await
                .expect("status")
                .mutation_revision,
            started.mutation_revision,
            "finishing an already-started mutation must not invalidate twice"
        );
    }

    #[tokio::test]
    async fn ignored_file_mutation_is_frozen_as_an_explicit_target() {
        let (temp, repo, ledger) = git_ledger_fixture().await;
        tokio::fs::write(repo.join(".gitignore"), "ignored.txt\n")
            .await
            .expect("ignore file");
        run_git(&repo, &["add", ".gitignore"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "ignore fixture",
            ],
        );
        tokio::fs::write(repo.join("ignored.txt"), "before")
            .await
            .expect("ignored baseline");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Set-Content ignored.txt after".to_string(),
        ];
        let mutation_guard = ledger
            .guard_normalized_command(&command, Some(&repo), "turn", false, false)
            .await
            .expect("ignored target authorization")
            .expect("managed mutation guard");
        tokio::fs::write(repo.join("ignored.txt"), "after")
            .await
            .expect("ignored mutation");
        ledger
            .record_command(
                &command,
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                true,
            )
            .await;
        drop(mutation_guard);

        let canonical_repo = dunce::canonicalize(&repo)
            .expect("canonical repo")
            .to_string_lossy()
            .into_owned();
        {
            let document = ledger.document.lock().await;
            let document = document.as_ref().expect("document");
            assert!(
                document
                    .task_changed_paths
                    .get(&canonical_repo)
                    .is_some_and(|paths| paths.contains("ignored.txt")),
                "ignored exact target must participate in closure path evidence"
            );
            assert!(
                document
                    .known_roots
                    .get(&canonical_repo)
                    .is_some_and(|state| !state.explicit_target_snapshots.is_empty())
            );
        }
        drop(temp);
    }

    #[tokio::test]
    async fn ignored_directory_mutation_is_frozen_recursively() {
        let (_temp, repo, ledger) = git_ledger_fixture().await;
        tokio::fs::write(repo.join(".gitignore"), "ignored/\n")
            .await
            .expect("ignore directory");
        run_git(&repo, &["add", ".gitignore"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "ignore directory fixture",
            ],
        );
        tokio::fs::create_dir_all(repo.join("ignored/nested"))
            .await
            .expect("ignored directory");
        tokio::fs::write(repo.join("ignored/nested/value.txt"), "before")
            .await
            .expect("ignored baseline");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Remove-Item ignored -Recurse".to_string(),
        ];
        let mutation_guard = ledger
            .guard_normalized_command(&command, Some(&repo), "turn", false, false)
            .await
            .expect("ignored directory authorization")
            .expect("managed mutation guard");
        tokio::fs::write(repo.join("ignored/nested/value.txt"), "after")
            .await
            .expect("ignored descendant mutation");
        ledger
            .record_command(
                &command,
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                true,
            )
            .await;
        drop(mutation_guard);

        let canonical_repo = dunce::canonicalize(&repo)
            .expect("canonical repo")
            .to_string_lossy()
            .into_owned();
        let document = ledger.document.lock().await;
        let document = document.as_ref().expect("document");
        assert!(
            document
                .task_changed_paths
                .get(&canonical_repo)
                .is_some_and(|paths| paths.contains("ignored")),
            "ignored directory descendants must participate in frozen closure state"
        );
    }

    #[tokio::test]
    async fn same_command_new_nested_repository_is_observed_as_changed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let nested = repo.join("nested");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&repo).await.expect("outer repo");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join(".gitignore"), "nested/\n")
            .await
            .expect("outer ignore");
        run_git(&repo, &["add", ".gitignore"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "outer fixture",
            ],
        );
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("outer repo");
        let ledger =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        ledger
            .begin_turn("turn", "create an ignored nested repository")
            .await
            .expect("begin turn");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "New-Item -ItemType Directory nested; git -C nested init; Set-Content nested/inner.txt changed"
                .to_string(),
        ];
        let mutation_guard = ledger
            .guard_normalized_command(&command, Some(&repo), "turn", false, false)
            .await
            .expect("pre-execution ownership")
            .expect("managed mutation guard");
        tokio::fs::create_dir_all(&nested).await.expect("nested");
        run_git(&nested, &["init", "--quiet"]);
        tokio::fs::write(nested.join("inner.txt"), "changed")
            .await
            .expect("nested file");
        run_git(&nested, &["add", "inner.txt"]);
        run_git(
            &nested,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "nested fixture",
            ],
        );
        ledger
            .record_command(&command, &PathUri::from_abs_path(&cwd), 0, false, 1, true)
            .await;
        drop(mutation_guard);

        let canonical_nested = dunce::canonicalize(&nested)
            .expect("canonical nested")
            .to_string_lossy()
            .into_owned();
        let document = ledger.document.lock().await;
        let document = document.as_ref().expect("document");
        assert!(
            document.observed_roots.contains(&canonical_nested),
            "nested root {canonical_nested} missing from {document:#?}"
        );
        assert!(
            document
                .task_changed_paths
                .get(&canonical_nested)
                .is_some_and(|paths| paths.contains(".")),
            "a repository created during the command must have an absent baseline"
        );
    }

    #[tokio::test]
    async fn computed_command_target_keeps_unknown_mutation_risk_open() {
        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "$target='ignored.txt'; Set-Content $target changed".to_string(),
        ];
        let mutation_guard = ledger
            .guard_normalized_command(&command, Some(&repo), "turn", false, false)
            .await
            .expect("bounded process authorization")
            .expect("managed mutation guard");
        tokio::fs::write(repo.join("ignored.txt"), "changed")
            .await
            .expect("computed mutation");
        ledger
            .record_command(
                &command,
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                true,
            )
            .await;
        drop(mutation_guard);

        let document = ledger.document.lock().await;
        assert!(
            document
                .as_ref()
                .expect("document")
                .risks
                .iter()
                .any(|risk| risk.source == "command" && !risk.resolved)
        );
    }

    #[tokio::test]
    async fn descendant_coverage_registers_ignored_nested_root_in_parent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("outer");
        let nested = repo.join("nested");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&repo).await.expect("outer repo");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join(".gitignore"), "nested/\n")
            .await
            .expect("outer ignore");
        tokio::fs::write(repo.join("outer.txt"), "outer")
            .await
            .expect("outer tracked file");
        run_git(&repo, &["add", "."]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "outer fixture",
            ],
        );
        tokio::fs::create_dir_all(&nested)
            .await
            .expect("nested repository");
        run_git(&nested, &["init", "--quiet"]);
        tokio::fs::write(nested.join("inner.txt"), "initial")
            .await
            .expect("nested tracked file");
        run_git(&nested, &["add", "inner.txt"]);
        run_git(
            &nested,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "nested fixture",
            ],
        );

        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute outer repo");
        let parent =
            TaskEvidenceLedger::load_or_new(codex_home.clone(), ThreadId::new(), cwd.as_path())
                .await;
        parent
            .begin_turn("parent-turn", "parent task")
            .await
            .expect("parent turn");
        parent
            .classify(ordinary_classification(&[]))
            .await
            .expect("parent classification");
        let child =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        child
            .begin_turn("child-turn", "child task")
            .await
            .expect("child turn");
        child
            .classify(ordinary_classification(&[]))
            .await
            .expect("child classification");

        let mutation_guard = child
            .guard_normalized_command(
                &[
                    "powershell".to_string(),
                    "-Command".to_string(),
                    "Set-Content nested/inner.txt changed".to_string(),
                ],
                Some(&repo),
                "child-turn",
                false,
                false,
            )
            .await
            .expect("child nested mutation should be authorized")
            .expect("child mutation guard");
        tokio::fs::write(nested.join("inner.txt"), "changed")
            .await
            .expect("mutate nested tracked file");
        drop(mutation_guard);

        let before = parent.inspect_status().await.expect("parent status");
        let coverage = child.descendant_coverage().await.expect("child coverage");
        parent
            .merge_descendant_coverage(std::slice::from_ref(&coverage))
            .await
            .expect("merge child coverage");
        let merged = parent.inspect_status().await.expect("merged parent status");
        let canonical_nested = dunce::canonicalize(&nested)
            .expect("canonical nested root")
            .to_string_lossy()
            .into_owned();
        assert!(merged.known_roots.contains(&canonical_nested));
        assert!(merged.mutation_revision > before.mutation_revision);
        let (_, _, changed_paths, _) = parent.snapshot_known_roots_and_task_changes().await;
        assert_eq!(
            changed_paths.get(&canonical_nested),
            Some(&BTreeSet::from(["inner.txt".to_string()]))
        );

        parent
            .merge_descendant_coverage(std::slice::from_ref(&coverage))
            .await
            .expect("identical child coverage retry");
        assert_eq!(
            parent
                .inspect_status()
                .await
                .expect("retried parent status")
                .mutation_revision,
            merged.mutation_revision
        );
    }

    #[tokio::test]
    async fn unavailable_git_ownership_is_rejected_and_terminates_blocked() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let nested_root = ledger.repo_root.as_ref().expect("root").join("unavailable");
        tokio::fs::create_dir_all(nested_root.join(".git"))
            .await
            .expect("invalid git marker");
        let command = vec![
            "powershell".to_string(),
            "-Command".to_string(),
            "Set-Content result.txt changed".to_string(),
        ];
        assert!(
            ledger
                .guard_normalized_command(&command, Some(&nested_root), "turn", false, false,)
                .await
                .is_err()
        );
        assert!(
            !ledger
                .authorize_final_item("turn", "premature")
                .await
                .expect("final authorization")
        );

        let first = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        let second = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("second closure");
        assert_eq!(second.phase, TaskPhase::Ready);
        assert_eq!(second.outcome, Some(TaskOutcome::Blocked));
    }

    #[tokio::test]
    async fn green_command_receipt_without_path_closure_cannot_pass() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let repo = ledger.repo_root.as_ref().expect("root");
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(
                    &AbsolutePathBuf::from_absolute_path(repo).expect("absolute root"),
                ),
                0,
                false,
                1,
                false,
            )
            .await;
        let receipts = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .rev()
            .take(1)
            .collect();

        let status = ledger
            .submit_closure(ClosureSubmission {
                path_review: BTreeSet::new(),
                competing_paths_checked: BTreeSet::new(),
                validation_receipt_ids: BTreeSet::new(),
                runtime_evidence: receipts,
                missing_requirement_ids: BTreeSet::new(),
                actionable_findings: BTreeSet::new(),
                blocked_reasons: BTreeSet::new(),
            })
            .await
            .expect("closure");
        assert_eq!(status.phase, TaskPhase::Fixing);
        assert!(status.outcome.is_none());
    }

    #[tokio::test]
    async fn duplicate_canonical_receipts_are_all_recognized_without_changing_the_hash_set() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| {
                invalidate_for_mutation(document);
                let receipt = ValidationReceipt {
                    id: "validation-duplicate-a".to_string(),
                    recorded_at: timestamp(),
                    epoch: document.evidence_epoch,
                    step_id: None,
                    mode: "final".to_string(),
                    verdict: Some("VERIFIED".to_string()),
                    tool_success: true,
                    proof_bearing: true,
                    accepted_proof: true,
                    active_files: Vec::new(),
                    stale_reasons: Vec::new(),
                    payload: None,
                };
                let mut duplicate = receipt.clone();
                duplicate.id = "validation-duplicate-b".to_string();
                document.validation_receipts.extend([receipt, duplicate]);
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;

        let mut closure = closure_with_missing(&[]);
        closure.path_review = BTreeSet::from(["kd4_features.toml".to_string()]);
        closure.competing_paths_checked = BTreeSet::from(["kd4_features.toml".to_string()]);
        closure.validation_receipt_ids = BTreeSet::from([
            "validation-duplicate-a".to_string(),
            "validation-duplicate-b".to_string(),
        ]);
        let status = ledger.submit_closure(closure).await.expect("closure");

        assert_eq!(status.phase, TaskPhase::Ready, "{}", status.message);
        assert_eq!(status.outcome, Some(TaskOutcome::Passed));
    }

    #[tokio::test]
    async fn unrelated_path_review_cannot_cover_a_task_changed_file() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_edit_intent("patch", &repo, &[PathBuf::from("kd4_features.toml")])
            .await;
        tokio::fs::write(repo.join("kd4_features.toml"), "# changed")
            .await
            .expect("write");
        ledger.record_edit_result("patch", "completed").await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.path_review = BTreeSet::from(["scripts/verify_local.py".to_string()]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .rev()
            .take(1)
            .collect();

        let status = ledger.submit_closure(closure).await.expect("closure");
        assert_eq!(status.phase, TaskPhase::Fixing);
        assert_eq!(status.incomplete_occurrences, 1);
    }

    #[tokio::test]
    async fn preexisting_dirty_path_is_not_claimed_as_a_task_change() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        tokio::fs::write(
            repo.join("scripts/verify_local.py"),
            "# user-owned dirty work",
        )
        .await
        .expect("preexisting dirty file");
        let baseline = snapshot_repository_state(&repo).await;
        let root = baseline.root.clone();
        let (_, snapshot) = ledger
            .update_document(|document| {
                document.known_roots = BTreeMap::from([(root.clone(), baseline.clone())]);
                document.root_baselines = BTreeMap::from([(root.clone(), baseline.clone())]);
                document.task_changed_paths.clear();
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_edit_intent("patch", &repo, &[PathBuf::from("kd4_features.toml")])
            .await;
        tokio::fs::write(repo.join("kd4_features.toml"), "# task change")
            .await
            .expect("task edit");
        ledger.record_edit_result("patch", "completed").await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;

        {
            let document = ledger.document.lock().await;
            let paths = document
                .as_ref()
                .and_then(|document| document.task_changed_paths.get(&root))
                .expect("task changed paths");
            assert!(paths.contains("kd4_features.toml"));
            assert!(!paths.contains("scripts/verify_local.py"));
        }
        let mut closure = closure_with_missing(&["risk:unassociated-edit-patch"]);
        closure.path_review = BTreeSet::from(["kd4_features.toml".to_string()]);
        closure.competing_paths_checked = BTreeSet::from(["kd4_features.toml".to_string()]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();
        let first = ledger
            .submit_closure(closure.clone())
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing, "{}", first.message);
        assert_eq!(first.incomplete_occurrences, 1);
        let status = ledger
            .submit_closure(closure)
            .await
            .expect("second closure");
        assert_eq!(status.phase, TaskPhase::Ready, "{}", status.message);
        assert_eq!(status.outcome, Some(TaskOutcome::Partial));
    }

    #[tokio::test]
    async fn review_selection_counts_all_roots_and_recognizes_agent_lifecycle_paths() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut document = ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .clone();
        document.classification = Some(ordinary_classification(&[]));
        document.task_changed_paths = BTreeMap::from([
            (
                "C:/repo-a".to_string(),
                (0..11).map(|index| format!("src/a{index}.rs")).collect(),
            ),
            (
                "C:/repo-b".to_string(),
                (0..10).map(|index| format!("src/b{index}.rs")).collect(),
            ),
        ]);
        assert!(
            review_required(&document),
            "broadness must be computed across every registered root"
        );

        document.task_changed_paths = BTreeMap::from([(
            "C:/repo-a".to_string(),
            BTreeSet::from(["core/src/agent/control/spawn.rs".to_string()]),
        )]);
        assert!(
            review_required(&document),
            "agent lifecycle changes require isolated review"
        );
        for path in [
            "core/src/session/handlers.rs",
            "core/src/tools/spec_plan.rs",
            "core/src/tools/handlers/task_state.rs",
            "core/src/exec.rs",
            "core/src/spawn.rs",
            "utils/pty/src/process.rs",
        ] {
            document.task_changed_paths =
                BTreeMap::from([("C:/repo-a".to_string(), BTreeSet::from([path.to_string()]))]);
            assert!(
                review_required(&document),
                "{path} must require isolated lifecycle review"
            );
        }
        for path in [
            "state/src/lib.rs",
            "codex-rs/thread-store/src/lib.rs",
            "message-history/src/lib.rs",
            "rollout/src/recorder.rs",
            "memories/src/lib.rs",
            "goals/src/lib.rs",
            "agent-graph-store/src/lib.rs",
            "agent-task-store/src/lib.rs",
        ] {
            document.task_changed_paths =
                BTreeMap::from([("C:/repo-a".to_string(), BTreeSet::from([path.to_string()]))]);
            assert!(
                review_required(&document),
                "{path} must require isolated persistence review"
            );
        }
        document.task_changed_paths = BTreeMap::from([(
            "C:/repo-a".to_string(),
            BTreeSet::from(["core/src/state_machine.rs".to_string()]),
        )]);
        assert!(
            !review_required(&document),
            "persistence owner matching must use exact path components"
        );
        document.task_changed_paths = BTreeMap::from([(
            "C:/repo-a".to_string(),
            BTreeSet::from(["core/src/ordinary.rs".to_string()]),
        )]);
        assert!(!review_required(&document));
    }

    #[tokio::test]
    async fn committed_protocol_change_still_requires_independent_review() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        tokio::fs::create_dir_all(repo.join("protocol"))
            .await
            .expect("protocol directory");
        tokio::fs::write(repo.join("protocol/schema.rs"), "pub const V: u8 = 1;")
            .await
            .expect("protocol file");
        run_git(&repo, &["add", "protocol/schema.rs"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "protocol change",
            ],
        );
        ledger
            .record_command(
                &["git".to_string(), "commit".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                true,
            )
            .await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.path_review = BTreeSet::from(["protocol/schema.rs".to_string()]);
        closure.competing_paths_checked = BTreeSet::from(["protocol/schema.rs".to_string()]);
        let runtime_receipts = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids;
        closure.runtime_evidence = runtime_receipts.into_iter().rev().take(1).collect();

        let status = ledger.submit_closure(closure).await.expect("closure");
        let gate = ledger.completion_gate().await;
        assert_eq!(
            status.phase,
            TaskPhase::Reviewing,
            "{}; gate={gate:?}",
            status.message
        );
        assert!(status.review_required);
    }

    #[tokio::test]
    async fn identical_incomplete_closure_without_verified_work_terminates_blocked() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;

        let closure = closure_with_missing(&["runtime-or-wiring-evidence"]);
        let first = ledger
            .submit_closure(closure.clone())
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        assert_eq!(first.incomplete_occurrences, 1);
        let accepted_revision = first.accepted_evidence_revision;
        let mutation_revision = first.mutation_revision;

        let second = ledger
            .submit_closure(closure)
            .await
            .expect("second closure");
        assert_eq!(second.phase, TaskPhase::Ready);
        assert_eq!(second.outcome, Some(TaskOutcome::Blocked));
        assert_eq!(second.incomplete_occurrences, 2);
        assert_eq!(second.accepted_evidence_revision, accepted_revision);
        assert_eq!(second.mutation_revision, mutation_revision);
    }

    #[tokio::test]
    async fn canonical_closure_hash_uses_runtime_derived_missing_requirements() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let mutation_guard = ledger
            .guard_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("mutation authorization")
            .expect("managed mutation guard");
        drop(mutation_guard);

        let first = ledger
            .submit_closure(closure_with_missing(&["runtime-or-wiring-evidence"]))
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        assert_eq!(first.incomplete_occurrences, 1);
        let second = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("same runtime-derived closure");
        assert_eq!(second.phase, TaskPhase::Ready);
        assert_eq!(second.outcome, Some(TaskOutcome::Blocked));
        assert_eq!(second.incomplete_occurrences, 2);
        assert_eq!(
            second.accepted_evidence_revision,
            first.accepted_evidence_revision
        );
        assert_eq!(second.closure_fingerprint, first.closure_fingerprint);
    }

    #[tokio::test]
    async fn canonical_closure_hash_still_changes_for_fresh_runtime_evidence() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| {
                invalidate_for_mutation(document);
                document
                    .unsupported_mutation_targets
                    .insert("outside".to_string());
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(
                    &AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo"),
                ),
                0,
                false,
                1,
                false,
            )
            .await;
        let first_runtime_receipt = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .last()
            .expect("first runtime receipt");
        let first = ledger
            .submit_closure(ClosureSubmission {
                runtime_evidence: BTreeSet::from([first_runtime_receipt]),
                ..closure_with_missing(&["supported-mutation-ownership"])
            })
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        assert_eq!(first.incomplete_occurrences, 1);

        ledger
            .record_command(
                &["cargo".to_string(), "check".to_string()],
                &PathUri::from_abs_path(
                    &AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo"),
                ),
                0,
                false,
                1,
                false,
            )
            .await;
        let second_runtime_receipt = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .last()
            .expect("second runtime receipt");
        let second = ledger
            .submit_closure(ClosureSubmission {
                runtime_evidence: BTreeSet::from([second_runtime_receipt]),
                ..closure_with_missing(&["supported-mutation-ownership"])
            })
            .await
            .expect("closure with distinct fresh runtime evidence");

        assert_eq!(second.phase, TaskPhase::Fixing);
        assert_eq!(second.incomplete_occurrences, 1);
        assert_eq!(
            second.accepted_evidence_revision,
            first.accepted_evidence_revision.saturating_add(1)
        );
        assert_ne!(second.closure_fingerprint, first.closure_fingerprint);
    }

    #[tokio::test]
    async fn high_risk_incomplete_closure_is_reviewed_without_losing_blocked_outcome() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&["lifecycle"]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;

        let closure = closure_with_missing(&["runtime-or-wiring-evidence"]);
        assert_eq!(
            ledger
                .submit_closure(closure.clone())
                .await
                .expect("first closure")
                .phase,
            TaskPhase::Fixing
        );
        let status = ledger
            .submit_closure(closure)
            .await
            .expect("second closure");
        assert_eq!(status.phase, TaskPhase::Reviewing);
        assert!(status.outcome.is_none());

        let packet = ledger
            .prepare_review()
            .await
            .expect("review preparation")
            .expect("review packet");
        let status = ledger
            .accept_review(
                &packet.binding_hash,
                review_receipt(BTreeSet::new(), "patch is correct"),
            )
            .await
            .expect("clean review");
        assert_eq!(status.phase, TaskPhase::Ready);
        assert_eq!(status.outcome, Some(TaskOutcome::Blocked));
        assert!(
            ledger
                .authorize_final_item("turn", "final")
                .await
                .expect("final authorization")
        );
    }

    #[tokio::test]
    async fn repeated_incomplete_review_terminates_in_an_authorizable_blocked_state() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        ledger
            .classify(ordinary_classification(&["lifecycle"]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();

        for attempt in 1..=2 {
            assert_eq!(
                ledger
                    .submit_closure(closure.clone())
                    .await
                    .expect("closure")
                    .phase,
                TaskPhase::Reviewing
            );
            let packet = ledger
                .prepare_review()
                .await
                .expect("review preparation")
                .expect("review packet");
            let status = ledger
                .accept_review(
                    &packet.binding_hash,
                    review_receipt(BTreeSet::new(), "review unavailable"),
                )
                .await
                .expect("review receipt");
            if attempt == 1 {
                assert_eq!(status.phase, TaskPhase::Fixing);
            } else {
                assert_eq!(status.phase, TaskPhase::Ready);
                assert_eq!(status.outcome, Some(TaskOutcome::Blocked));
            }
        }
        assert!(
            ledger
                .authorize_final_item("turn", "final")
                .await
                .expect("final authorization")
        );
    }

    #[tokio::test]
    async fn review_findings_reopen_and_clean_review_reaches_ready_directly() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        ledger
            .classify(ordinary_classification(&["lifecycle"]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();
        let status = ledger.submit_closure(closure).await.expect("closure");
        assert_eq!(status.phase, TaskPhase::Reviewing);
        let packet = ledger
            .prepare_review()
            .await
            .expect("review preparation")
            .expect("review packet");
        let status = ledger
            .accept_review(
                &packet.binding_hash,
                review_receipt(
                    BTreeSet::from(["finding: stale competing path".to_string()]),
                    "patch is incorrect",
                ),
            )
            .await
            .expect("finding");
        assert_eq!(status.phase, TaskPhase::Fixing);
        assert!(status.outcome.is_none());
        assert_eq!(status.accepted_evidence_revision, 2);
        assert_eq!(
            ledger
                .submit_closure(closure_with_missing(&[]))
                .await
                .expect("unrepaired closure")
                .phase,
            TaskPhase::Fixing
        );

        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();
        assert_eq!(
            ledger
                .submit_closure(closure)
                .await
                .expect("second closure")
                .phase,
            TaskPhase::Reviewing
        );
        let packet = ledger
            .prepare_review()
            .await
            .expect("review preparation")
            .expect("review packet");
        let status = ledger
            .accept_review(
                &packet.binding_hash,
                review_receipt(BTreeSet::new(), "patch is correct"),
            )
            .await
            .expect("clean review");
        assert_eq!(status.phase, TaskPhase::Ready);
        assert_eq!(status.outcome, Some(TaskOutcome::Passed));
        assert!(!status.review_required);
    }

    #[tokio::test]
    async fn drift_at_closure_review_and_final_authority_boundaries_reopens_fixing() {
        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        tokio::fs::write(repo.join("tracked.txt"), "closure drift")
            .await
            .expect("drift");
        let status = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("closure drift result");
        assert_eq!(status.phase, TaskPhase::Fixing);
        assert_eq!(status.mutation_revision, 1);
        assert_eq!(status.accepted_evidence_revision, 0);

        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&["lifecycle"]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(
                    &AbsolutePathBuf::from_absolute_path(&repo).expect("absolute root"),
                ),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();
        assert_eq!(
            ledger.submit_closure(closure).await.expect("closure").phase,
            TaskPhase::Reviewing
        );
        tokio::fs::write(repo.join("tracked.txt"), "review-start drift")
            .await
            .expect("drift");
        assert!(
            ledger
                .prepare_review()
                .await
                .expect("review preparation")
                .is_none()
        );
        assert_eq!(
            ledger.inspect_status().await.expect("status").phase,
            TaskPhase::Fixing
        );

        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&["lifecycle"]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(
                    &AbsolutePathBuf::from_absolute_path(&repo).expect("absolute root"),
                ),
                0,
                false,
                1,
                false,
            )
            .await;
        let mut closure = closure_with_missing(&[]);
        closure.runtime_evidence = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();
        ledger.submit_closure(closure).await.expect("closure");
        let packet = ledger
            .prepare_review()
            .await
            .expect("review preparation")
            .expect("review packet");
        tokio::fs::write(repo.join("tracked.txt"), "review receipt drift")
            .await
            .expect("drift");
        let status = ledger
            .accept_review(
                &packet.binding_hash,
                review_receipt(BTreeSet::new(), "patch is correct"),
            )
            .await
            .expect("review receipt");
        assert_eq!(status.phase, TaskPhase::Fixing);

        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        tokio::fs::write(repo.join("tracked.txt"), "final drift")
            .await
            .expect("drift");
        assert!(
            !ledger
                .authorize_final_item("turn", "final")
                .await
                .expect("final authorization")
        );
        assert_eq!(
            ledger.inspect_status().await.expect("status").phase,
            TaskPhase::Fixing
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_mutation_and_final_authorization_are_serialized() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| {
                invalidate_for_mutation(document);
                force_ready_for_test(document, TaskOutcome::Passed);
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;

        assert!(
            ledger
                .authorize_final_item("turn", "final-item")
                .await
                .expect("final reservation")
        );
        ledger
            .mark_final_item_persisted(
                "turn",
                "final-item",
                &codex_protocol::models::ResponseItem::Message {
                    id: Some("final-item".to_string()),
                    role: "assistant".to_string(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: "final".to_string(),
                    }],
                    phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await
            .expect("persist final");
        let apply_patch_tool = ToolName::plain("apply_patch");
        let (final_result, mutation_result) = tokio::join!(
            ledger.commit_final_item("turn", "final-item"),
            ledger
                .guard_tool_dispatch(&apply_patch_tool, "turn", false, false, false, false, true,),
        );
        let final_authorized = final_result.expect("final authorization");
        assert_ne!(
            final_authorized,
            mutation_result.is_ok(),
            "exactly one side of the Ready mutation/final race must win"
        );
    }

    #[tokio::test]
    async fn repository_override_resets_repo_evidence_but_preserves_pending_final_ids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        let codex_home = temp.path().join("home");
        initialize_test_git_repo(&repo_a, "repository a").await;
        initialize_test_git_repo(&repo_b, "repository b").await;
        let thread_id = ThreadId::new();
        let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo_a).await;
        ledger
            .begin_turn("turn", "repository override task")
            .await
            .expect("begin turn");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(
                    &AbsolutePathBuf::from_absolute_path(&repo_a).expect("absolute repo"),
                ),
                0,
                false,
                1,
                false,
            )
            .await;
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        reserve_persisted_final_for_test(&ledger, "cross-root-provisional", "provisional").await;
        drop(ledger);

        let moved = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo_b).await;
        assert_eq!(
            moved.protected_pending_final_item_ids().await,
            BTreeSet::from(["cross-root-provisional".to_string()])
        );
        assert!(
            !moved
                .commit_final_item("turn", "cross-root-provisional")
                .await
                .expect("stale provisional commit")
        );
        {
            let document = moved.document.lock().await;
            let document = document.as_ref().expect("document");
            assert_eq!(
                document.start.repository_root,
                repo_b.to_string_lossy().into_owned()
            );
            assert_eq!(
                document
                    .known_roots
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([repo_b.to_string_lossy().into_owned()])
            );
            assert_eq!(document.mutation_revision, 0);
            assert!(document.classification.is_none());
            assert!(document.command_receipts.is_empty());
            assert!(document.accepted_closure.is_none());
            let pending = document
                .pending_finals
                .iter()
                .find(|pending| pending.item_id == "cross-root-provisional")
                .expect("preserved pending final");
            assert!(pending.persisted);
            assert!(pending.superseded);
            assert!(!pending.emission_reserved);
            assert!(pending.emission_key.is_empty());
            assert!(pending.emission_items.is_empty());
        }
        assert_no_preserved_task_evidence(&codex_home).await;
    }

    #[tokio::test]
    async fn repository_override_preserves_incomplete_committed_outbox() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        let codex_home = temp.path().join("home");
        initialize_test_git_repo(&repo_a, "repository a").await;
        initialize_test_git_repo(&repo_b, "repository b").await;
        let thread_id = ThreadId::new();
        let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo_a).await;
        ledger
            .begin_turn("turn", "repository override task")
            .await
            .expect("begin turn");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        reserve_persisted_final_for_test(&ledger, "cross-root-final", "committed final").await;
        assert!(
            ledger
                .commit_final_item("turn", "cross-root-final")
                .await
                .expect("commit final")
        );
        let before = ledger
            .recoverable_final_emission()
            .await
            .expect("recoverable final")
            .expect("committed final");
        drop(ledger);

        let moved = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo_b).await;
        let after = moved
            .recoverable_final_emission()
            .await
            .expect("recoverable moved final")
            .expect("preserved committed final");
        assert_eq!(after.turn_id, before.turn_id);
        assert_eq!(after.item_id, before.item_id);
        assert_eq!(after.emission_key, before.emission_key);
        assert_eq!(
            serde_json::to_value(&after.items).expect("serialize moved items"),
            serde_json::to_value(&before.items).expect("serialize original items")
        );
        assert!(serialized_values_equal(
            &after.terminal_event,
            &before.terminal_event
        ));
        assert_eq!(after.terminal_event_staged, before.terminal_event_staged);
        assert_eq!(
            moved.protected_pending_final_item_ids().await,
            BTreeSet::from(["cross-root-final".to_string()])
        );
        let error = moved
            .begin_turn("next-turn", "must wait for committed final recovery")
            .await
            .expect_err("incomplete committed final must fence a new turn");
        assert!(error.contains("final emission is incomplete"), "{error}");
        {
            let document = moved.document.lock().await;
            let document = document.as_ref().expect("document");
            assert_eq!(
                document.start.repository_root,
                repo_b.to_string_lossy().into_owned()
            );
            assert_eq!(document.mutation_revision, 0);
            assert!(document.classification.is_none());
            assert!(document.accepted_closure.is_none());
        }
        assert_no_preserved_task_evidence(&codex_home).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_steering_and_final_commit_are_serialized() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        assert!(
            ledger
                .authorize_final_item("turn", "final-item")
                .await
                .expect("final reservation")
        );
        ledger
            .mark_final_item_persisted(
                "turn",
                "final-item",
                &codex_protocol::models::ResponseItem::Message {
                    id: Some("final-item".to_string()),
                    role: "assistant".to_string(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: "final".to_string(),
                    }],
                    phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await
            .expect("persist final");

        let (final_result, steering_result) = tokio::join!(
            ledger.commit_final_item("turn", "final-item"),
            ledger.extend_task_contract("turn", "late steering"),
        );
        let final_committed = final_result.expect("final commit");
        let steering_result = steering_result.expect("steering serialization");
        assert_eq!(
            (final_committed, steering_result),
            if final_committed {
                (true, TaskContractUpdate::FinalCommitted)
            } else {
                (false, TaskContractUpdate::Extended)
            }
        );
    }

    #[tokio::test]
    async fn plan_only_final_staging_requires_one_bound_nonblank_matching_plan() {
        const RAW_FINAL: &str = concat!(
            "before\n<proposed_plan>\n",
            "- inspect the exact path",
            "<oai-mem-citation>memory source</oai-mem-citation>\n",
            "</proposed_plan>\nafter"
        );
        const PLAN_TEXT: &str = "- inspect the exact path\n";

        let (_temp, ledger) = ledger_fixture().await;
        reserve_persisted_final_for_test(&ledger, "plan-final", RAW_FINAL).await;

        let valid_plan = TurnItem::Plan(codex_protocol::items::PlanItem {
            id: "turn-plan".to_string(),
            text: PLAN_TEXT.to_string(),
        });
        let malformed_batches = [
            ("empty batch", Vec::new()),
            (
                "wrong plan id",
                vec![TurnItem::Plan(codex_protocol::items::PlanItem {
                    id: "other-turn-plan".to_string(),
                    text: PLAN_TEXT.to_string(),
                })],
            ),
            (
                "blank plan",
                vec![TurnItem::Plan(codex_protocol::items::PlanItem {
                    id: "turn-plan".to_string(),
                    text: " \n\t".to_string(),
                })],
            ),
            (
                "plan text not bound to the persisted response",
                vec![TurnItem::Plan(codex_protocol::items::PlanItem {
                    id: "turn-plan".to_string(),
                    text: "- a different plan\n".to_string(),
                })],
            ),
            (
                "multiple plans",
                vec![valid_plan.clone(), valid_plan.clone()],
            ),
        ];
        for (label, items) in malformed_batches {
            ledger
                .stage_final_emission_items("turn", "plan-final", &items)
                .await
                .expect_err(label);
        }

        {
            let document = ledger.document.lock().await;
            let pending = document
                .as_ref()
                .expect("document")
                .pending_finals
                .iter()
                .find(|pending| pending.item_id == "plan-final")
                .expect("pending final");
            assert!(pending.emission_key.is_empty());
            assert!(pending.emission_items.is_empty());
            assert!(pending.emission_reserved);
            assert!(!pending.superseded);
        }

        let emission_key = ledger
            .stage_final_emission_items("turn", "plan-final", &[valid_plan])
            .await
            .expect("valid plan-only batch");
        assert!(!emission_key.is_empty());
        let document = ledger.document.lock().await;
        let pending = document
            .as_ref()
            .expect("document")
            .pending_finals
            .iter()
            .find(|pending| pending.item_id == "plan-final")
            .expect("pending final");
        assert_eq!(pending.emission_items.len(), 1);
        let TurnItem::Plan(plan) = &pending.emission_items[0] else {
            panic!("expected the staged plan item");
        };
        assert_eq!(plan.id, "turn-plan");
        assert_eq!(plan.text, PLAN_TEXT);
    }

    #[tokio::test]
    async fn abort_final_reservation_is_durable_and_idempotent() {
        const RAW_FINAL: &str = "<proposed_plan>\n- inspect the exact path\n</proposed_plan>";
        const PLAN_TEXT: &str = "- inspect the exact path\n";

        let (_temp, ledger) = ledger_fixture().await;
        reserve_persisted_final_for_test(&ledger, "plan-final", RAW_FINAL).await;
        ledger
            .stage_final_emission_items(
                "turn",
                "plan-final",
                &[TurnItem::Plan(codex_protocol::items::PlanItem {
                    id: "turn-plan".to_string(),
                    text: PLAN_TEXT.to_string(),
                })],
            )
            .await
            .expect("stage final");
        assert_eq!(
            ledger.managed_final_state_for_turn("turn").await,
            Some(ManagedFinalState::AwaitingCommit)
        );

        ledger
            .abort_final_reservation("turn", "plan-final")
            .await
            .expect("first abort");
        let state_after_first_abort = {
            let document = ledger.document.lock().await;
            let pending = document
                .as_ref()
                .expect("document")
                .pending_finals
                .iter()
                .find(|pending| pending.item_id == "plan-final")
                .expect("pending final");
            (
                pending.emission_reserved,
                pending.superseded,
                pending.persisted,
                pending.externally_emitted,
                pending.externally_completed,
                pending.emission_key.clone(),
                pending.emission_items.len(),
            )
        };
        assert!(!state_after_first_abort.0);
        assert!(state_after_first_abort.1);
        assert!(state_after_first_abort.2);
        assert!(!state_after_first_abort.3);
        assert!(!state_after_first_abort.4);

        ledger
            .abort_final_reservation("turn", "plan-final")
            .await
            .expect("repeated abort");
        let state_after_second_abort = {
            let document = ledger.document.lock().await;
            let pending = document
                .as_ref()
                .expect("document")
                .pending_finals
                .iter()
                .find(|pending| pending.item_id == "plan-final")
                .expect("pending final");
            (
                pending.emission_reserved,
                pending.superseded,
                pending.persisted,
                pending.externally_emitted,
                pending.externally_completed,
                pending.emission_key.clone(),
                pending.emission_items.len(),
            )
        };
        assert_eq!(state_after_second_abort, state_after_first_abort);
        assert_eq!(
            ledger.managed_final_state_for_turn("turn").await,
            Some(ManagedFinalState::NoFinalCandidate)
        );
        assert!(
            !ledger
                .commit_final_item("turn", "plan-final")
                .await
                .expect("aborted final commit")
        );
        assert!(
            ledger
                .recoverable_final_emission()
                .await
                .expect("recoverable final")
                .is_none()
        );

        let evidence_path = ledger.evidence_path.as_ref().expect("evidence path");
        let persisted: TaskEvidenceDocument = serde_json::from_slice(
            &tokio::fs::read(evidence_path)
                .await
                .expect("read persisted evidence"),
        )
        .expect("decode persisted evidence");
        let pending = persisted
            .pending_finals
            .iter()
            .find(|pending| pending.item_id == "plan-final")
            .expect("persisted pending final");
        assert!(!pending.emission_reserved);
        assert!(pending.superseded);
    }

    #[tokio::test]
    async fn active_user_shell_is_rejected_without_mutating_task_evidence() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let before = ledger.inspect_status().await.expect("status");

        let err = ledger
            .guard_active_turn_user_shell("turn")
            .await
            .expect_err("managed active-turn /shell must fail closed");
        assert!(
            err.contains("cannot be strongly contained"),
            "unexpected error: {err}"
        );
        let after = ledger.inspect_status().await.expect("status");
        assert_eq!(after.mutation_revision, before.mutation_revision);
        assert_eq!(after.phase, before.phase);
        assert_eq!(
            ledger.active_mutations.load(Ordering::Acquire),
            0,
            "rejected /shell must not acquire mutation authority"
        );
    }

    #[tokio::test]
    async fn ready_mutation_reopens_and_advances_revision_atomically() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| {
                invalidate_for_mutation(document);
                force_ready_for_test(document, TaskOutcome::Passed);
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let before = ledger
            .inspect_status()
            .await
            .expect("status")
            .mutation_revision;

        ledger
            .guard_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("mutation authorization");

        let after = ledger.inspect_status().await.expect("status");
        assert_eq!(after.phase, TaskPhase::Fixing);
        assert!(after.outcome.is_none());
        assert_eq!(after.mutation_revision, before + 1);
    }

    #[tokio::test]
    async fn provisional_final_marker_survives_serialization() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| invalidate_for_mutation(document))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        assert!(
            !ledger
                .authorize_final_item("turn", "provisional")
                .await
                .expect("authorization")
        );
        ledger
            .mark_final_item_persisted(
                "turn",
                "provisional",
                &codex_protocol::models::ResponseItem::Message {
                    id: Some("provisional".to_string()),
                    role: "assistant".to_string(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: "provisional".to_string(),
                    }],
                    phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await
            .expect("persist provisional state");

        let document = ledger.document.lock().await.clone().expect("document");
        let encoded = serde_json::to_vec(&document).expect("serialize");
        let decoded: TaskEvidenceDocument = serde_json::from_slice(&encoded).expect("deserialize");
        assert!(decoded.pending_finals.iter().any(|pending| {
            pending.turn_id == "turn"
                && pending.item_id == "provisional"
                && pending.persisted
                && !pending.externally_emitted
                && !pending.superseded
        }));

        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        assert!(
            ledger
                .authorize_final_item("turn", "valid-final")
                .await
                .expect("valid final")
        );
        ledger
            .mark_final_item_persisted(
                "turn",
                "valid-final",
                &codex_protocol::models::ResponseItem::Message {
                    id: Some("valid-final".to_string()),
                    role: "assistant".to_string(),
                    content: vec![codex_protocol::models::ContentItem::OutputText {
                        text: "valid final".to_string(),
                    }],
                    phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await
            .expect("persist valid final state");
        assert!(
            ledger
                .commit_final_item("turn", "valid-final")
                .await
                .expect("commit valid final")
        );
        {
            let document = ledger.document.lock().await;
            let selected = document
                .as_ref()
                .expect("document")
                .pending_finals
                .iter()
                .find(|pending| pending.item_id == "valid-final")
                .expect("selected final");
            assert!(!selected.externally_emitted);
            assert!(!selected.externally_completed);
        }
        ledger
            .mark_final_item_completed("turn", "valid-final")
            .await
            .expect("complete final");
        assert!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .pending_finals
                .iter()
                .all(|pending| {
                    !pending.persisted || pending.externally_emitted || pending.superseded
                })
        );
    }

    #[tokio::test]
    async fn persisted_closing_phase_recovers_to_fixing() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut document = ledger.document.lock().await.clone().expect("document");
        document.classification = Some(ordinary_classification(&[]));
        document.phase = TaskPhase::Closing;
        document.outcome = Some(TaskOutcome::Passed);
        document.accepted_closure = Some(AcceptedClosure {
            task_generation: document.task_generation,
            task_contract_hash: sha1_hex(document.task_contract.as_bytes()),
            receipt_hash: "incomplete".to_string(),
            mutation_revision: 1,
            accepted_evidence_revision: 1,
            frozen_diff_hash: "stale".to_string(),
            terminal_outcome: Some(TaskOutcome::Passed),
            missing_requirement_ids: BTreeSet::new(),
            validation_receipt_hashes: BTreeSet::new(),
            runtime_evidence_hashes: BTreeSet::new(),
            review_required: true,
        });
        document.clean_review_hash = Some("stale-review".to_string());

        migrate_document(&mut document);

        assert_eq!(document.phase, TaskPhase::Fixing);
        assert!(document.outcome.is_none());
        assert!(document.accepted_closure.is_none());
        assert!(document.clean_review_hash.is_none());
    }

    #[tokio::test]
    async fn schema_v2_migration_requires_classification_and_preserves_mutation_revision() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut document = ledger.document.lock().await.clone().expect("document");
        document.schema_version = 2;
        document.evidence_epoch = 4;
        document.phase = TaskPhase::Ready;
        document.outcome = Some(TaskOutcome::Passed);
        document.mutation_revision = 0;

        migrate_document(&mut document);

        assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(document.phase, TaskPhase::Unclassified);
        assert!(document.outcome.is_none());
        assert_eq!(document.mutation_revision, 4);
    }

    #[tokio::test]
    async fn schema_v2_load_seeds_a_conservative_primary_root_baseline() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&repo).await.expect("repo");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join("tracked.txt"), "initial")
            .await
            .expect("tracked file");
        run_git(&repo, &["add", "tracked.txt"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let thread_id = ThreadId::new();
        let ledger = TaskEvidenceLedger::load_or_new(codex_home.clone(), thread_id, &repo).await;
        let evidence_path = ledger.evidence_path.clone().expect("evidence path");
        let mut legacy = ledger.document.lock().await.clone().expect("document");
        legacy.schema_version = 2;
        legacy.evidence_epoch = 3;
        legacy.mutation_revision = 0;
        legacy.known_roots.clear();
        legacy.root_baselines.clear();
        legacy.task_changed_paths.clear();
        tokio::fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&legacy).expect("serialize"),
        )
        .await
        .expect("write legacy evidence");
        drop(ledger);

        let migrated = TaskEvidenceLedger::load_or_new(codex_home, thread_id, &repo).await;
        let (_, _, changed_paths, _) = migrated.snapshot_known_roots_and_task_changes().await;
        let document = migrated.document.lock().await;
        let document = document.as_ref().expect("document");

        assert_eq!(document.mutation_revision, 3);
        assert_eq!(document.known_roots.len(), 1);
        assert!(
            document
                .root_baselines
                .values()
                .all(|state| !state.available)
        );
        assert!(
            changed_paths
                .values()
                .all(|paths| paths == &BTreeSet::from([".".to_string()]))
        );
    }

    #[tokio::test]
    async fn schema_v6_migration_clears_stale_termination_state() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut document = ledger.document.lock().await.clone().expect("document");
        document.schema_version = 6;
        document.classification = Some(ordinary_classification(&[]));
        document.closure_fingerprint = Some("stale".to_string());
        document
            .incomplete_occurrences
            .insert("stale".to_string(), 1);
        document
            .review_attempt_failures
            .insert("stale-review".to_string(), 1);
        let generation = document.task_generation;

        migrate_document(&mut document);

        assert_eq!(document.schema_version, TASK_EVIDENCE_SCHEMA_VERSION);
        assert_eq!(document.phase, TaskPhase::Fixing);
        assert!(document.closure_fingerprint.is_none());
        assert!(document.incomplete_occurrences.is_empty());
        assert!(document.review_attempt_failures.is_empty());
        assert_eq!(document.task_generation, generation + 1);
    }

    #[tokio::test]
    async fn migration_reopens_ready_state_with_mismatched_terminal_outcome() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut document = ledger.document.lock().await.clone().expect("document");
        document.classification = Some(ordinary_classification(&[]));
        force_ready_for_test(&mut document, TaskOutcome::Passed);
        document.outcome = Some(TaskOutcome::Partial);

        migrate_document(&mut document);

        assert_eq!(document.phase, TaskPhase::Fixing);
        assert!(document.outcome.is_none());
        assert!(document.accepted_closure.is_none());
    }

    #[tokio::test]
    async fn supported_non_git_drift_invalidates_frozen_closure_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("non-git");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&root)
            .await
            .expect("non-git root");
        let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), &root).await;
        ledger
            .begin_turn("turn", "non-Git mutation")
            .await
            .expect("begin turn");
        ledger
            .classify(TaskClassification {
                supported_non_git_roots: BTreeSet::from([root.to_string_lossy().into_owned()]),
                ..ordinary_classification(&[])
            })
            .await
            .expect("classification");
        ledger
            .register_mutation_targets(&root, &[root.join("result.txt")])
            .await
            .expect("register target");
        let reservation = ledger
            .guard_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("mutation guard")
            .expect("managed mutation");
        tokio::fs::write(root.join("result.txt"), "first")
            .await
            .expect("first write");
        ledger
            .finish_reserved_tool_dispatch("turn", &reservation)
            .await
            .expect("finish mutation");
        drop(reservation);
        let before = ledger.inspect_status().await.expect("status");

        tokio::fs::write(root.join("result.txt"), "external drift")
            .await
            .expect("external write");
        let status = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("closure");

        assert_eq!(status.phase, TaskPhase::Fixing);
        assert_eq!(status.mutation_revision, before.mutation_revision + 1);
        assert!(status.message.contains("non-Git drift"));
    }

    #[tokio::test]
    async fn resolved_derived_actionable_risk_allows_fresh_closure() {
        let (temp, ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| {
                invalidate_for_mutation(document);
                upsert_risk(
                    document,
                    EvidenceRisk {
                        id: "retryable-validation".to_string(),
                        description: "retryable validation failed".to_string(),
                        source: "verify_local".to_string(),
                        blocking: true,
                        resolved: false,
                        epoch: document.evidence_epoch,
                    },
                );
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        ledger
            .record_command(
                &["cargo".to_string(), "test".to_string()],
                &PathUri::from_abs_path(&AbsolutePathBuf::from_absolute_path(&repo).expect("repo")),
                0,
                false,
                1,
                false,
            )
            .await;
        let runtime: BTreeSet<String> = ledger
            .inspect_status()
            .await
            .expect("status")
            .command_receipt_ids
            .into_iter()
            .collect();
        let first = ledger
            .submit_closure(ClosureSubmission {
                runtime_evidence: runtime.clone(),
                missing_requirement_ids: BTreeSet::from(
                    [("risk:retryable-validation".to_string())],
                ),
                ..closure_with_missing(&[])
            })
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        assert_eq!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .latest_actionable_finding_revision,
            None
        );

        let (_, snapshot) = ledger
            .update_document(|document| resolve_risk(document, "retryable-validation"))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let second = ledger
            .submit_closure(ClosureSubmission {
                runtime_evidence: runtime,
                ..closure_with_missing(&[])
            })
            .await
            .expect("second closure");

        assert_eq!(second.phase, TaskPhase::Ready, "{}", second.message);
        assert_eq!(second.outcome, Some(TaskOutcome::Passed));
    }

    #[tokio::test]
    async fn task_contract_change_resets_incomplete_occurrence_handling() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| {
                invalidate_for_mutation(document);
                document
                    .unsupported_mutation_targets
                    .insert("outside".to_string());
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        let first = ledger
            .submit_closure(ClosureSubmission {
                runtime_evidence: BTreeSet::from(["runtime evidence".to_string()]),
                missing_requirement_ids: BTreeSet::from([
                    ("supported-mutation-ownership".to_string())
                ]),
                ..closure_with_missing(&[])
            })
            .await
            .expect("closure");
        assert_eq!(first.incomplete_occurrences, 1);

        ledger
            .extend_task_contract("turn", "additional required behavior")
            .await
            .expect("extend contract");
        let document = ledger.document.lock().await;
        assert!(
            document
                .as_ref()
                .expect("document")
                .incomplete_occurrences
                .is_empty()
        );
    }

    #[tokio::test]
    async fn first_classification_records_preclassification_git_drift() {
        let (_temp, repo, ledger) = git_ledger_fixture().await;
        tokio::fs::write(repo.join("tracked.txt"), "changed before classification")
            .await
            .expect("write");

        let status = ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, _, changed_paths, _) = ledger.snapshot_known_roots_and_task_changes().await;

        assert_eq!(status.mutation_revision, 1);
        assert!(
            changed_paths
                .values()
                .any(|paths| paths.contains("tracked.txt"))
        );
    }

    #[tokio::test]
    async fn first_classification_registers_a_newly_created_git_root_as_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(&repo).await.expect("repo");
        let ledger = TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), &repo).await;
        ledger
            .begin_turn("turn", "repository appears before classification")
            .await
            .expect("begin turn");
        run_git(&repo, &["init", "--quiet"]);
        tokio::fs::write(repo.join("tracked.txt"), "new repository")
            .await
            .expect("tracked file");
        run_git(&repo, &["add", "tracked.txt"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.name=KD4 Test",
                "-c",
                "user.email=kd4@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );

        let status = ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, _, changed_paths, _) = ledger.snapshot_known_roots_and_task_changes().await;
        let document = ledger.document.lock().await;
        let document = document.as_ref().expect("document");

        assert_eq!(status.mutation_revision, 1);
        assert_eq!(document.observed_roots.len(), 1);
        assert!(
            changed_paths
                .values()
                .all(|paths| paths == &BTreeSet::from([".".to_string()]))
        );
    }

    #[tokio::test]
    async fn freshness_and_root_drift_advance_one_mutation_revision() {
        let (_temp, repo, ledger) = git_ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let original = snapshot_file(&repo, "tracked.txt").await;
        let (_, snapshot) = ledger
            .update_document(|document| {
                document
                    .latest_file_hashes
                    .insert("tracked.txt".to_string(), original);
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        tokio::fs::write(repo.join("tracked.txt"), "external change")
            .await
            .expect("write");
        let before = ledger.inspect_status().await.expect("status");

        let after = ledger
            .submit_closure(closure_with_missing(&[]))
            .await
            .expect("closure");

        assert_eq!(after.mutation_revision, before.mutation_revision + 1);
    }

    #[tokio::test]
    async fn staging_only_transition_is_attributed_to_its_path() {
        let (_temp, repo, _ledger) = git_ledger_fixture().await;
        tokio::fs::write(repo.join("tracked.txt"), "updated")
            .await
            .expect("write");
        let unstaged = snapshot_repository_state(&repo).await;
        run_git(&repo, &["add", "tracked.txt"]);
        let staged = snapshot_repository_state(&repo).await;

        let changed = task_changed_paths_from_baseline(&repo, Some(&unstaged), &staged).await;

        assert_eq!(changed, BTreeSet::from(["tracked.txt".to_string()]));
    }

    #[tokio::test]
    async fn classification_escalation_is_fenced_by_active_mutation() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let reservation = ledger
            .reserve_tool_dispatch(
                &ToolName::plain("apply_patch"),
                "turn",
                false,
                false,
                false,
                false,
                true,
            )
            .await
            .expect("reservation")
            .expect("managed mutation");

        let err = ledger
            .classify(TaskClassification {
                exhaustive: true,
                ..ordinary_classification(&[])
            })
            .await
            .expect_err("classification must be fenced");
        assert!(err.contains("mutation is in flight"), "{err}");
        drop(reservation);
    }

    #[tokio::test]
    async fn protected_pending_final_ids_include_superseded_incomplete_items() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(ordinary_classification(&[]))
            .await
            .expect("classification");
        let (_, snapshot) = ledger
            .update_document(|document| force_ready_for_test(document, TaskOutcome::Passed))
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;
        assert!(
            ledger
                .authorize_final_item("turn", "superseded")
                .await
                .expect("reserve")
        );
        let (_, snapshot) = ledger
            .update_document(|document| {
                let pending = document.pending_finals.last_mut().expect("pending");
                pending.superseded = true;
                let mut completed = pending.clone();
                completed.item_id = "completed".to_string();
                completed.externally_emitted = true;
                completed.externally_completed = true;
                completed.superseded = false;
                let mut completed_without_emission = pending.clone();
                completed_without_emission.item_id = "completed-without-emission".to_string();
                completed_without_emission.externally_completed = true;
                completed_without_emission.superseded = false;
                document.pending_finals.push(completed);
                document.pending_finals.push(completed_without_emission);
            })
            .await
            .expect("document");
        ledger.persist_document(&snapshot).await;

        assert_eq!(
            ledger.protected_pending_final_item_ids().await,
            BTreeSet::from([
                "completed-without-emission".to_string(),
                "superseded".to_string(),
            ])
        );
    }
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
