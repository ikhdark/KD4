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
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_tools::ToolName;
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
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::warn;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 4;
const FILE_HASH_CHUNK_SIZE: usize = 64 * 1024;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_VALIDATION_RECEIPTS: usize = 64;
const MAX_EXTERNAL_EVIDENCE_RECEIPTS: usize = 256;
const EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES: usize = 16 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES: usize = 8 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_HEADER: &str =
    "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1\n";

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
    active_mutations: Arc<AtomicU64>,
    #[cfg(test)]
    persistence_test_control: Arc<std::sync::Mutex<Option<PersistenceTestControl>>>,
}

#[derive(Debug)]
pub(crate) struct TaskMutationGuard {
    active_mutations: Arc<AtomicU64>,
}

impl Drop for TaskMutationGuard {
    fn drop(&mut self) {
        let previous = self.active_mutations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "task mutation guard underflow");
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskClassification {
    #[serde(default)]
    pub(crate) exhaustive: bool,
    #[serde(default)]
    pub(crate) risk_domains: BTreeSet<String>,
    #[serde(default)]
    pub(crate) supported_non_git_roots: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InvestigationCheckpoint {
    #[serde(default)]
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) paths_reviewed: BTreeSet<String>,
    #[serde(default)]
    pub(crate) competing_paths_checked: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClosureSubmission {
    #[serde(default)]
    pub(crate) path_review: BTreeSet<String>,
    #[serde(default)]
    pub(crate) competing_paths_checked: BTreeSet<String>,
    #[serde(default)]
    pub(crate) validation_receipt_ids: BTreeSet<String>,
    #[serde(default)]
    pub(crate) runtime_evidence: BTreeSet<String>,
    #[serde(default)]
    pub(crate) missing_requirement_ids: BTreeSet<String>,
    #[serde(default)]
    pub(crate) actionable_findings: BTreeSet<String>,
    #[serde(default)]
    pub(crate) blocked_reasons: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskLifecycleStatus {
    pub(crate) phase: TaskPhase,
    pub(crate) outcome: Option<TaskOutcome>,
    pub(crate) mutation_revision: u64,
    pub(crate) accepted_evidence_revision: Option<u64>,
    pub(crate) review_required: bool,
    pub(crate) closure_fingerprint: Option<String>,
    pub(crate) incomplete_occurrences: u8,
    pub(crate) validation_receipt_ids: Vec<String>,
    pub(crate) command_receipt_ids: Vec<String>,
    pub(crate) message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskEvidenceReviewPacket {
    pub(crate) prompt: String,
    pub(crate) binding_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskReviewReceipt {
    pub(crate) findings: Vec<String>,
    pub(crate) verdict: String,
    pub(crate) explanation: String,
    pub(crate) confidence_score_millis: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TaskLifecycleState {
    #[serde(default)]
    phase: TaskPhase,
    #[serde(default)]
    outcome: Option<TaskOutcome>,
    #[serde(default)]
    classification: Option<TaskClassification>,
    #[serde(default)]
    investigation_checkpoint: Option<InvestigationCheckpoint>,
    #[serde(default)]
    accepted_evidence_revision: Option<u64>,
    #[serde(default)]
    accepted_closure: Option<ClosureSubmission>,
    #[serde(default)]
    review_required: bool,
    #[serde(default)]
    prepared_review_binding: Option<String>,
    #[serde(default)]
    clean_review_hash: Option<String>,
    #[serde(default)]
    closure_fingerprint: Option<String>,
    #[serde(default)]
    incomplete_evidence_marker: Option<String>,
    #[serde(default)]
    incomplete_occurrences: u8,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TaskEvidenceValidationStart {
    epoch: u64,
    file_snapshots: BTreeMap<String, FileHashSnapshot>,
    owned_file_paths: BTreeSet<String>,
    artifact_snapshots: BTreeMap<String, FileHashSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistOutcome {
    Persisted,
    Superseded,
    Failed,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
struct PersistenceTestControl {
    before_next_write:
        Arc<std::sync::Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>>,
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
    validation_receipts: Vec<ValidationReceipt>,
    #[serde(default)]
    external_evidence: Vec<ExternalEvidenceReceipt>,
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
    #[serde(default = "initial_receipt_sequence")]
    next_external_evidence_receipt_sequence: u64,
    #[serde(default)]
    host_mutation_revision: u64,
    #[serde(default)]
    lifecycle: TaskLifecycleState,
    completion: Option<TaskCompletionGate>,
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
    active_files: Vec<FileHashSnapshot>,
    stale_reasons: Vec<String>,
    payload: Option<Value>,
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
    recorded_at: String,
    task_epoch: u64,
    step_id: Option<String>,
    workspace_root_fingerprint: String,
    host_mutation_revision: Option<u64>,
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
        let existing = match existing {
            ExistingDocument::Loaded(document) => Some(*document),
            ExistingDocument::Missing => None,
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
                None
            }
        };
        let document = if let Some(mut document) = existing {
            migrate_document(&mut document);
            document.start.repository_root.clone_from(&repository_root);
            document.updated_at = now;
            document.revision = document.revision.saturating_add(1);
            document
        } else {
            let git = collect_git_info(&repo_root).await;
            TaskEvidenceDocument {
                schema_version: TASK_EVIDENCE_SCHEMA_VERSION,
                revision: 1,
                thread_id: thread_id_text,
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
                external_evidence: Vec::new(),
                generated_artifact_requirements: Vec::new(),
                generated_artifact_hashes: BTreeMap::new(),
                latest_generated_artifact_hashes: BTreeMap::new(),
                latest_file_hashes: BTreeMap::new(),
                risks: storage_failure_reason
                    .as_deref()
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
                next_external_evidence_receipt_sequence: initial_receipt_sequence(),
                host_mutation_revision: 0,
                lifecycle: TaskLifecycleState::default(),
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
        let ledger = Self {
            mode,
            codex_home: Some(codex_home),
            thread_id: Some(thread_id.to_string()),
            evidence_path: writable_evidence_path,
            repo_root: Some(repo_root),
            document: Arc::new(Mutex::new(Some(document.clone()))),
            persistence_gate: Arc::new(Semaphore::new(1)),
            external_evidence_gate: Arc::new(Semaphore::new(1)),
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            active_mutations: Arc::new(AtomicU64::new(0)),
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
        }
        let referenced_artifact_ids = document
            .external_evidence
            .iter()
            .filter_map(|receipt| receipt.payload_artifact_id.clone())
            .collect();
        let live_artifact_ids =
            crate::tools::command_output_artifact::reconcile_evidence_artifact_protection(
                ledger
                    .codex_home
                    .as_deref()
                    .expect("enabled ledger has a Codex home"),
                ledger
                    .thread_id
                    .as_deref()
                    .expect("enabled ledger has a thread id"),
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
            active_mutations: Arc::new(AtomicU64::new(0)),
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

    pub(crate) fn matches_repo_root(&self, candidate: &Path) -> bool {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        repository_roots_match(repo_root, candidate)
    }

    pub(crate) async fn inspect_status(&self) -> Option<TaskLifecycleStatus> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let guard = self.document.lock().await;
        guard.as_ref().map(|document| {
            status_from_document(
                document,
                if document.lifecycle.message.is_empty() {
                    "task lifecycle state is available"
                } else {
                    &document.lifecycle.message
                },
            )
        })
    }

    pub(crate) async fn classify(
        &self,
        mut classification: TaskClassification,
    ) -> Result<TaskLifecycleStatus, String> {
        if !self.allows_kd4_completion() {
            return Err("task evidence is disabled".to_string());
        }
        classification.risk_domains = classification
            .risk_domains
            .into_iter()
            .map(|domain| domain.trim().to_ascii_lowercase())
            .filter(|domain| !domain.is_empty())
            .collect();
        let mut normalized_roots = BTreeSet::new();
        for root in classification.supported_non_git_roots {
            let path = PathBuf::from(root.trim());
            if !path.is_absolute() {
                return Err("supported_non_git_roots entries must be absolute paths".to_string());
            }
            normalized_roots.insert(
                canonical_repository_root(&path)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        classification.supported_non_git_roots = normalized_roots;

        let Some((result, snapshot)) = self
            .update_document(|document| {
                if let Some(existing) = document.lifecycle.classification.as_ref() {
                    if existing != &classification {
                        return Err(
                            "task classification is immutable after the lifecycle has started"
                                .to_string(),
                        );
                    }
                    return Ok(status_from_document(
                        document,
                        "task classification was already recorded",
                    ));
                }
                if document.lifecycle.phase != TaskPhase::Unclassified {
                    return Err("task classification is no longer available".to_string());
                }
                document.lifecycle.classification = Some(classification.clone());
                document.lifecycle.phase = if classification.exhaustive {
                    TaskPhase::Investigating
                } else {
                    TaskPhase::Fixing
                };
                document.lifecycle.outcome = None;
                document.lifecycle.message = if classification.exhaustive {
                    "classification recorded; an investigation checkpoint is required before mutation"
                        .to_string()
                } else {
                    "classification recorded; repository mutation is enabled".to_string()
                };
                document.updated_at = timestamp();
                Ok(status_from_document(
                    document,
                    &document.lifecycle.message,
                ))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = result?;
        self.persist_required(&snapshot, "recording task classification")
            .await?;
        Ok(status)
    }

    pub(crate) async fn submit_investigation_checkpoint(
        &self,
        mut checkpoint: InvestigationCheckpoint,
    ) -> Result<TaskLifecycleStatus, String> {
        if !self.allows_kd4_completion() {
            return Err("task evidence is disabled".to_string());
        }
        checkpoint.summary = checkpoint.summary.trim().to_string();
        checkpoint.paths_reviewed = normalized_nonempty_strings(checkpoint.paths_reviewed);
        checkpoint.competing_paths_checked =
            normalized_nonempty_strings(checkpoint.competing_paths_checked);
        if checkpoint.summary.is_empty() || checkpoint.paths_reviewed.is_empty() {
            return Err(
                "an investigation checkpoint requires a summary and at least one reviewed path"
                    .to_string(),
            );
        }
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                let Some(classification) = document.lifecycle.classification.as_ref() else {
                    return Err(
                        "classify the task before submitting an investigation checkpoint"
                            .to_string(),
                    );
                };
                if !classification.exhaustive {
                    return Err(
                        "this task was not classified for exhaustive investigation".to_string()
                    );
                }
                if active_mutations.load(Ordering::Acquire) != 0 {
                    return Err(
                        "the investigation checkpoint cannot commit while a mutation is active"
                            .to_string(),
                    );
                }
                if document.lifecycle.phase == TaskPhase::Fixing
                    && document.lifecycle.investigation_checkpoint.as_ref() == Some(&checkpoint)
                {
                    return Ok(status_from_document(
                        document,
                        "investigation checkpoint was already recorded",
                    ));
                }
                if document.lifecycle.phase != TaskPhase::Investigating {
                    return Err(format!(
                        "investigation checkpoints require the investigating phase, not {:?}",
                        document.lifecycle.phase
                    ));
                }
                document.lifecycle.investigation_checkpoint = Some(checkpoint);
                document.lifecycle.phase = TaskPhase::Fixing;
                document.lifecycle.message =
                    "investigation checkpoint accepted; repository mutation is enabled".to_string();
                document.updated_at = timestamp();
                Ok(status_from_document(document, &document.lifecycle.message))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = result?;
        self.persist_required(&snapshot, "recording investigation checkpoint")
            .await?;
        Ok(status)
    }

    pub(crate) async fn reserve_tool_dispatch(
        &self,
        tool_name: &ToolName,
        turn_id: &str,
        declared_read_only: bool,
        review_delegate: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        if !self.allows_kd4_completion() {
            return Ok(None);
        }
        let name = tool_name.name.as_str();
        if name == "task_state" {
            return Ok(None);
        }
        if review_delegate && !declared_read_only {
            return Err(format!(
                "independent review delegates cannot invoke mutating tool `{name}`"
            ));
        }
        if declared_read_only {
            return Ok(None);
        }
        if matches!(name, "shell_command" | "exec_command" | "write_stdin") {
            return Ok(None);
        }
        if name == "verify_local" {
            return self
                .reserve_mutation(
                    &format!("validation tool `{name}` in turn `{turn_id}`"),
                    None,
                    review_delegate,
                    false,
                )
                .await;
        }
        self.reserve_mutation(
            &format!("tool `{name}` in turn `{turn_id}`"),
            None,
            review_delegate,
            name != "apply_patch",
        )
        .await
    }

    pub(crate) async fn guard_normalized_command(
        &self,
        command: &[String],
        cwd: Option<&Path>,
        turn_id: &str,
        review_delegate: bool,
        trusted_validation: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        if !self.allows_kd4_completion() || is_known_safe_command(command) {
            return Ok(None);
        }
        self.reserve_mutation(
            &format!(
                "{} command in turn `{turn_id}`",
                if trusted_validation {
                    "validation"
                } else {
                    "shell"
                }
            ),
            cwd,
            review_delegate,
            false,
        )
        .await
    }

    pub(crate) async fn guard_named_mutation(
        &self,
        mutation_name: &str,
        turn_id: &str,
        review_delegate: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        self.reserve_mutation(
            &format!("`{mutation_name}` in turn `{turn_id}`"),
            None,
            review_delegate,
            true,
        )
        .await
    }

    async fn reserve_mutation(
        &self,
        label: &str,
        cwd: Option<&Path>,
        review_delegate: bool,
        invalidate_at_start: bool,
    ) -> Result<Option<TaskMutationGuard>, String> {
        if !self.allows_kd4_completion() {
            return Ok(None);
        }
        if review_delegate {
            return Err(format!("independent review delegates cannot start {label}"));
        }
        let mut guard = self.document.lock().await;
        let Some(document) = guard.as_mut() else {
            return Err("task evidence is disabled".to_string());
        };
        if document.lifecycle.classification.is_none() {
            return Err(format!(
                "{label} is blocked until `task_state.classify` records the task policy"
            ));
        }
        if document.lifecycle.phase != TaskPhase::Fixing {
            return Err(format!(
                "{label} requires the fixing phase; current phase is {:?}",
                document.lifecycle.phase
            ));
        }
        if let Some(cwd) = cwd
            && !self.mutation_path_is_supported(document, cwd)
        {
            return Err(format!(
                "{label} targets `{}` outside the repository and declared non-Git roots",
                cwd.display()
            ));
        }
        self.active_mutations.fetch_add(1, Ordering::AcqRel);
        let snapshot = if invalidate_at_start {
            invalidate_for_mutation(document);
            document.updated_at = timestamp();
            document.revision = document.revision.saturating_add(1);
            Some(document.clone())
        } else {
            None
        };
        drop(guard);
        if let Some(snapshot) = snapshot
            && let Err(err) = self
                .persist_required(&snapshot, "reserving a task mutation")
                .await
        {
            self.active_mutations.fetch_sub(1, Ordering::AcqRel);
            return Err(err);
        }
        Ok(Some(TaskMutationGuard {
            active_mutations: Arc::clone(&self.active_mutations),
        }))
    }

    fn mutation_path_is_supported(
        &self,
        document: &TaskEvidenceDocument,
        candidate: &Path,
    ) -> bool {
        let candidate = canonical_repository_root(candidate);
        if self
            .repo_root
            .as_ref()
            .is_some_and(|root| path_is_within_root(&candidate, root))
        {
            return true;
        }
        document
            .lifecycle
            .classification
            .as_ref()
            .into_iter()
            .flat_map(|classification| classification.supported_non_git_roots.iter())
            .map(PathBuf::from)
            .any(|root| path_is_within_root(&candidate, &root))
    }

    pub(crate) async fn submit_closure(
        &self,
        mut closure: ClosureSubmission,
    ) -> Result<TaskLifecycleStatus, String> {
        if !self.allows_kd4_completion() {
            return Err("task evidence is disabled".to_string());
        }
        closure.path_review = normalized_nonempty_strings(closure.path_review);
        closure.competing_paths_checked =
            normalized_nonempty_strings(closure.competing_paths_checked);
        closure.validation_receipt_ids =
            normalized_nonempty_strings(closure.validation_receipt_ids);
        closure.runtime_evidence = normalized_nonempty_strings(closure.runtime_evidence);
        closure.missing_requirement_ids =
            normalized_nonempty_strings(closure.missing_requirement_ids);
        closure.actionable_findings = normalized_nonempty_strings(closure.actionable_findings);
        closure.blocked_reasons = normalized_nonempty_strings(closure.blocked_reasons);

        let gate = self.completion_gate().await.unwrap_or(TaskCompletionGate {
            status: TaskCompletionStatus::Partial,
            reasons: vec!["no tracked task evidence is available".to_string()],
            evidence_path: self
                .evidence_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        });
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if document.lifecycle.classification.is_none() {
                    return Err("classify the task before submitting closure evidence".to_string());
                }
                if active_mutations.load(Ordering::Acquire) != 0 {
                    return Err(
                        "closure cannot commit while a repository mutation is active".to_string(),
                    );
                }
                if document.lifecycle.phase != TaskPhase::Fixing {
                    return Err(format!(
                        "closure requires the fixing phase, not {:?}",
                        document.lifecycle.phase
                    ));
                }
                document.lifecycle.phase = TaskPhase::Closing;

                if !closure.actionable_findings.is_empty() {
                    document.lifecycle.phase = TaskPhase::Fixing;
                    document.lifecycle.outcome = None;
                    document.lifecycle.accepted_evidence_revision = None;
                    document.lifecycle.accepted_closure = None;
                    document.lifecycle.prepared_review_binding = None;
                    document.lifecycle.message = format!(
                        "closure reported actionable findings: {}",
                        closure
                            .actionable_findings
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("; ")
                    );
                    document.updated_at = timestamp();
                    return Ok(status_from_document(
                        document,
                        &document.lifecycle.message,
                    ));
                }

                let issues = closure_evidence_issues(document, &closure, &gate);
                let fingerprint = closure_fingerprint(document, &closure, &gate, &issues);
                let evidence_marker = verified_work_marker(document);
                if !issues.is_empty() {
                    let unchanged = document.lifecycle.closure_fingerprint.as_ref()
                        == Some(&fingerprint)
                        && document.lifecycle.incomplete_evidence_marker.as_ref()
                            == Some(&evidence_marker);
                    document.lifecycle.incomplete_occurrences = if unchanged {
                        document.lifecycle.incomplete_occurrences.saturating_add(1)
                    } else {
                        1
                    };
                    document.lifecycle.closure_fingerprint = Some(fingerprint);
                    document.lifecycle.incomplete_evidence_marker = Some(evidence_marker);
                    document.lifecycle.review_required = false;
                    document.lifecycle.prepared_review_binding = None;
                    document.lifecycle.clean_review_hash = None;
                    document.lifecycle.accepted_closure = Some(closure.clone());
                    document.lifecycle.accepted_evidence_revision = None;
                    document.lifecycle.outcome = None;
                    if unchanged && document.lifecycle.incomplete_occurrences >= 2 {
                        document.lifecycle.phase = TaskPhase::Ready;
                        document.lifecycle.outcome = Some(TaskOutcome::Blocked);
                        document.lifecycle.accepted_evidence_revision =
                            Some(document.host_mutation_revision);
                        document.lifecycle.message =
                            "identical incomplete closure without new verified work terminated as blocked"
                                .to_string();
                    } else {
                        document.lifecycle.phase = TaskPhase::Fixing;
                        document.lifecycle.message = format!(
                            "closure evidence is incomplete: {}",
                            issues.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
                        );
                    }
                    document.updated_at = timestamp();
                    return Ok(status_from_document(
                        document,
                        &document.lifecycle.message,
                    ));
                }

                document.lifecycle.accepted_closure = Some(closure);
                document.lifecycle.accepted_evidence_revision =
                    Some(document.host_mutation_revision);
                document.lifecycle.closure_fingerprint = Some(fingerprint);
                document.lifecycle.incomplete_evidence_marker = None;
                document.lifecycle.incomplete_occurrences = 0;
                document.lifecycle.review_required = lifecycle_review_required(document);
                document.lifecycle.prepared_review_binding = None;
                document.lifecycle.clean_review_hash = None;
                document.lifecycle.outcome = None;
                if document.lifecycle.review_required {
                    document.lifecycle.phase = TaskPhase::Reviewing;
                    document.lifecycle.message =
                        "closure evidence accepted; independent read-only review is required"
                            .to_string();
                } else {
                    document.lifecycle.phase = TaskPhase::Ready;
                    document.lifecycle.outcome = Some(TaskOutcome::Passed);
                    document.lifecycle.message =
                        "closure evidence accepted; final output is authorized".to_string();
                }
                document.updated_at = timestamp();
                Ok(status_from_document(
                    document,
                    &document.lifecycle.message,
                ))
            })
            .await
        else {
            return Err("task evidence is disabled".to_string());
        };
        let status = result?;
        self.persist_required(&snapshot, "committing closure evidence")
            .await?;
        Ok(status)
    }

    pub(crate) async fn prepare_review(&self) -> Result<Option<TaskEvidenceReviewPacket>, String> {
        if !self.allows_kd4_completion() {
            return Ok(None);
        }
        let gate = self.completion_gate().await.ok_or_else(|| {
            "independent review cannot start without tracked completion evidence".to_string()
        })?;
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if document.lifecycle.phase != TaskPhase::Reviewing {
                    return Ok(None);
                }
                if active_mutations.load(Ordering::Acquire) != 0 {
                    return Err(
                        "independent review cannot start while a mutation is active".to_string()
                    );
                }
                if gate.status != TaskCompletionStatus::Passed
                    || document.lifecycle.accepted_evidence_revision
                        != Some(document.host_mutation_revision)
                {
                    document.lifecycle.phase = TaskPhase::Fixing;
                    document.lifecycle.accepted_evidence_revision = None;
                    document.lifecycle.prepared_review_binding = None;
                    document.lifecycle.message =
                        "closure evidence drifted before independent review".to_string();
                    return Ok(None);
                }
                let binding_hash = review_binding_hash(document, &gate);
                document.lifecycle.prepared_review_binding = Some(binding_hash.clone());
                document.lifecycle.message =
                    "independent review packet prepared for the exact accepted revision"
                        .to_string();
                let prompt = task_evidence_review_prompt(document, &gate);
                Ok(Some(TaskEvidenceReviewPacket {
                    prompt,
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

    pub(crate) async fn accept_review(
        &self,
        binding_hash: &str,
        receipt: TaskReviewReceipt,
    ) -> Result<TaskLifecycleStatus, String> {
        if !self.allows_kd4_completion() {
            return Err("task evidence is disabled".to_string());
        }
        let gate = self.completion_gate().await.ok_or_else(|| {
            "independent review cannot commit without tracked completion evidence".to_string()
        })?;
        let active_mutations = Arc::clone(&self.active_mutations);
        let Some((result, snapshot)) = self
            .update_document(|document| {
                if document.lifecycle.phase != TaskPhase::Reviewing {
                    return Err(
                        "independent review no longer matches the lifecycle phase".to_string()
                    );
                }
                if active_mutations.load(Ordering::Acquire) != 0
                    || gate.status != TaskCompletionStatus::Passed
                    || document.lifecycle.accepted_evidence_revision
                        != Some(document.host_mutation_revision)
                {
                    document.lifecycle.phase = TaskPhase::Fixing;
                    document.lifecycle.accepted_evidence_revision = None;
                    document.lifecycle.prepared_review_binding = None;
                    document.lifecycle.message =
                        "independent review was invalidated by repository drift".to_string();
                    return Ok(status_from_document(document, &document.lifecycle.message));
                }
                if document.lifecycle.prepared_review_binding.as_deref() != Some(binding_hash)
                    || review_binding_hash(document, &gate) != binding_hash
                {
                    return Err(
                        "independent review receipt did not match the prepared evidence revision"
                            .to_string(),
                    );
                }
                let clean = receipt.findings.is_empty()
                    && receipt
                        .verdict
                        .trim()
                        .eq_ignore_ascii_case("patch is correct")
                    && !receipt.explanation.trim().is_empty()
                    && receipt.confidence_score_millis <= 1000;
                let receipt_hash = format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&serde_json::json!({
                            "binding": binding_hash,
                            "findings": receipt.findings,
                            "verdict": receipt.verdict,
                            "explanation": receipt.explanation,
                            "confidence": receipt.confidence_score_millis,
                        }))
                        .expect("review receipt serialization cannot fail")
                    )
                );
                document.lifecycle.prepared_review_binding = None;
                if clean {
                    document.lifecycle.clean_review_hash = Some(receipt_hash);
                    document.lifecycle.phase = TaskPhase::Ready;
                    document.lifecycle.outcome = Some(TaskOutcome::Passed);
                    document.lifecycle.message =
                        "clean independent review authorized final output".to_string();
                } else {
                    document.lifecycle.clean_review_hash = None;
                    document.lifecycle.phase = TaskPhase::Fixing;
                    document.lifecycle.outcome = None;
                    document.lifecycle.accepted_evidence_revision = None;
                    document.lifecycle.message =
                        "independent review found issues or returned a non-authorizing verdict"
                            .to_string();
                }
                document.updated_at = timestamp();
                Ok(status_from_document(document, &document.lifecycle.message))
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
        _turn_id: &str,
        _item_id: &str,
    ) -> Result<bool, String> {
        if !self.allows_kd4_completion() {
            return Ok(true);
        }
        if self.active_mutations.load(Ordering::Acquire) != 0 {
            return Ok(false);
        }
        let gate = self.completion_gate().await;
        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return Ok(true);
        };
        if self.active_mutations.load(Ordering::Acquire) != 0 {
            return Ok(false);
        }
        if document.host_mutation_revision == 0
            && document.lifecycle.classification.is_none()
            && document.lifecycle.phase == TaskPhase::Unclassified
        {
            return Ok(true);
        }
        if document.lifecycle.phase != TaskPhase::Ready
            || document.lifecycle.accepted_evidence_revision
                != Some(document.host_mutation_revision)
        {
            return Ok(false);
        }
        match document.lifecycle.outcome {
            Some(TaskOutcome::Blocked) => Ok(true),
            Some(TaskOutcome::Passed) => {
                Ok(gate.is_some_and(|gate| gate.status == TaskCompletionStatus::Passed))
            }
            Some(TaskOutcome::Partial) | None => Ok(false),
        }
    }

    pub(crate) async fn begin_verify_local_validation(
        &self,
        requested_paths: &[PathBuf],
    ) -> Option<TaskEvidenceValidationStart> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let repo_root = self.repo_root.as_ref()?;
        let requested_paths = requested_paths
            .iter()
            .map(|path| normalize_input_path(repo_root, Some(repo_root), path))
            .collect::<BTreeSet<_>>();
        let (epoch, mut owned_file_paths, artifact_paths) = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            let artifact_paths = document
                .generated_artifact_requirements
                .iter()
                .filter_map(|requirement| requirement.path.clone())
                .collect::<BTreeSet<_>>();
            (
                document.evidence_epoch,
                task_owned_file_paths(document),
                artifact_paths,
            )
        };
        owned_file_paths.extend(requested_paths);
        let mut file_paths = owned_file_paths.clone();
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
            owned_file_paths,
            artifact_snapshots,
        })
    }

    pub(crate) async fn try_record_plan_update(
        &self,
        update: &UpdatePlanArgs,
    ) -> Result<UpdatePlanArgs, String> {
        if !self.allows_kd4_completion() {
            return Ok(update.clone());
        }
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
                    invalidate_for_plan_change(document);
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
            let mut guard = self.document.lock().await;
            let Some(document) = guard.as_mut() else {
                return Err("plan update could not be persisted durably".to_string());
            };
            if document.revision == snapshot.revision {
                *document = previous_document;
            } else {
                document.lifecycle.phase = TaskPhase::Fixing;
                document.lifecycle.outcome = None;
                document.lifecycle.accepted_evidence_revision = None;
                document.lifecycle.accepted_closure = None;
                document.lifecycle.prepared_review_binding = None;
                document.lifecycle.message =
                    "plan invalidation could not be persisted durably".to_string();
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
        if !self.allows_kd4_completion() {
            return false;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        let normalized_active_files = active_files
            .iter()
            .map(|path| normalize_input_path(repo_root, Some(repo_root), path))
            .collect::<Vec<_>>();
        let mut checked_paths = normalized_active_files
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(start) = validation_start {
            checked_paths.extend(start.owned_file_paths.iter().cloned());
        }
        let mut validation_end_files = BTreeMap::new();
        for path in checked_paths {
            validation_end_files.insert(path.clone(), snapshot_file(repo_root, &path).await);
        }
        let file_snapshots = normalized_active_files
            .iter()
            .filter_map(|path| validation_end_files.get(path).cloned())
            .collect::<Vec<_>>();
        let mut validation_start_files = BTreeMap::new();
        let mut validation_end_artifacts = BTreeMap::new();
        if let Some(start) = validation_start {
            for path in validation_end_files.keys() {
                if let Some(snapshot) = start.file_snapshots.get(path) {
                    validation_start_files.insert(path.clone(), snapshot.clone());
                }
            }
            for path in start.artifact_snapshots.keys() {
                validation_end_artifacts.insert(path.clone(), snapshot_file(repo_root, path).await);
            }
        }
        let snapshots_unchanged = validation_start.is_some_and(|start| {
            normalized_active_files
                .iter()
                .all(|path| start.file_snapshots.contains_key(path))
                && validation_start_files == validation_end_files
                && start.artifact_snapshots == validation_end_artifacts
        });

        let Some((accepted_proof, snapshot)) = self
            .update_document(|document| {
                let run_matches_start = validation_start.is_some_and(|start| {
                    start.epoch == document.evidence_epoch && snapshots_unchanged
                });
                let accepted_proof = proof_bearing && tool_success && run_matches_start;
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
                    active_files: file_snapshots.clone(),
                    stale_reasons: stale_reasons.to_vec(),
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
                        let edit_free_step_is_ready = step.edit_paths.is_empty()
                            && matches!(
                                step.status,
                                StepStatus::Implemented
                                    | StepStatus::Completed
                                    | StepStatus::Passed
                            );
                        let edited_step_is_covered = !step.edit_paths.is_empty()
                            && step.edit_paths.iter().all(|path| {
                                file_snapshots.iter().any(|active| {
                                    active.read_error.is_none()
                                        && path_is_covered(path, &active.path)
                                })
                            });
                        if edit_free_step_is_ready || edited_step_is_covered {
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
        if !self.allows_kd4_completion() {
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
        if !self.allows_kd4_completion() {
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
        if !self.allows_kd4_completion() {
            return None;
        }
        let mut latest_gate = None;
        for _ in 0..8 {
            self.refresh_external_file_freshness().await;
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
        if !self.allows_kd4_completion() {
            return;
        }
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

    async fn refresh_external_file_freshness(&self) {
        if !self.allows_kd4_completion() {
            return;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let (expected, expected_artifacts) = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| {
                    (
                        document.latest_file_hashes.clone(),
                        document.generated_artifact_hashes.clone(),
                    )
                })
                .unwrap_or_default()
        };
        if expected.is_empty() && expected_artifacts.is_empty() {
            return;
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
                let changed_artifacts = changed_artifacts
                    .into_iter()
                    .filter(|(previous, current)| {
                        document.generated_artifact_hashes.get(&current.path) == Some(previous)
                    })
                    .map(|(_, current)| current)
                    .collect::<Vec<_>>();
                if changed.is_empty() && changed_artifacts.is_empty() {
                    return;
                }
                invalidate_for_mutation(document);
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
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
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
            MAX_EXTERNAL_EVIDENCE_RECEIPTS,
        )
        .await
    }

    async fn record_external_mcp_evidence_with_limit(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
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
        if self.evidence_path.is_none() {
            return ExternalEvidenceCapture::Warning(
                "external evidence persistence is unavailable for this task",
            );
        }
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
            let artifact_bytes = encode_external_evidence_artifact(&canonical_bytes);
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
        let evidence_path = self
            .evidence_path
            .clone()
            .expect("external evidence requires a persistence path");
        let codex_home = self.codex_home.clone();
        let thread_id = self.thread_id.clone();
        let mode = self.mode;
        let server_name = server_name.to_string();
        let tool_name = tool_name.to_string();
        let call_id = call_id.to_string();
        let tool_success = result.is_error != Some(true);
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
                    recorded_at: timestamp(),
                    task_epoch,
                    step_id,
                    workspace_root_fingerprint,
                    host_mutation_revision,
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

    async fn persist_required(
        &self,
        document: &TaskEvidenceDocument,
        operation: &str,
    ) -> Result<(), String> {
        match self.persist_document(document).await {
            PersistOutcome::Persisted | PersistOutcome::Superseded => Ok(()),
            PersistOutcome::Failed => Err(format!(
                "{operation} failed because task evidence could not be persisted durably"
            )),
        }
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
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
        }
        #[cfg(not(test))]
        None
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
        if let Some(control) = test_control.as_ref() {
            if let Some((started, release)) = control
                .before_next_write
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                started.wait();
                release.wait();
            }
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

fn encode_external_evidence_artifact(canonical_bytes: &[u8]) -> Vec<u8> {
    let canonical = std::str::from_utf8(canonical_bytes)
        .expect("canonical JSON serialization always produces valid UTF-8");
    let mut encoded = Vec::with_capacity(canonical_bytes.len() + 256);
    encoded.extend_from_slice(EXTERNAL_EVIDENCE_ARTIFACT_HEADER.as_bytes());
    let mut start = 0;
    while start < canonical.len() {
        let mut end = (start + EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES).min(canonical.len());
        while !canonical.is_char_boundary(end) {
            end -= 1;
        }
        let line = serde_json::to_string(&canonical[start..end])
            .expect("string serialization cannot fail");
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
        start = end;
    }
    encoded
}

fn workspace_root_fingerprint(start: &TaskStartState) -> String {
    let identity = canonicalize_json_value(serde_json::json!({
        "repositoryRoot": start.repository_root,
        "repositoryUrl": start.repository_url,
    }));
    let bytes =
        serde_json::to_vec(&identity).expect("workspace identity serialization cannot fail");
    format!("{:x}", Sha256::digest(bytes))
}

enum ExistingDocument {
    Missing,
    Loaded(Box<TaskEvidenceDocument>),
    Rejected { kind: &'static str, reason: String },
}

async fn load_existing_document(
    path: &Path,
    expected_thread_id: &str,
    expected_repository_root: &Path,
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
    if !recorded_repository_root_matches(&document.start.repository_root, expected_repository_root)
    {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: "repository root does not match the requested checkout".to_string(),
        };
    }
    ExistingDocument::Loaded(Box::new(document))
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

fn migrate_document(document: &mut TaskEvidenceDocument) {
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
            step.validation_receipt_ids.clear();
            if step.status == StepStatus::Passed {
                step.status = StepStatus::Implemented;
            }
        }
    }
    sync_plan_structure_state(document, &duplicate_step_ids);
    promote_steps_with_fresh_evidence(document);
    if document.lifecycle.accepted_evidence_revision.is_some()
        && document.lifecycle.accepted_evidence_revision != Some(document.host_mutation_revision)
    {
        invalidate_lifecycle_for_new_work(
            document,
            "stored lifecycle evidence did not match the current mutation revision",
        );
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

fn find_kd4_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|candidate| {
            candidate.join("scripts").join("verify_local.py").is_file()
                && candidate.join("kd4_features.toml").is_file()
        })
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
    for receipt in &document.validation_receipts {
        paths.extend(receipt.active_files.iter().map(|file| file.path.clone()));
    }
    paths
}

async fn git_dirty_paths(repo_root: &Path) -> BTreeSet<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await;
    let Ok(output) = output else {
        return BTreeSet::new();
    };
    if !output.status.success() {
        return BTreeSet::new();
    }
    parse_git_porcelain_paths(&output.stdout)
}

fn parse_git_porcelain_paths(output: &[u8]) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut records = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' {
            return BTreeSet::new();
        }
        insert_git_porcelain_path(&mut paths, &record[3..]);
        if record[..2]
            .iter()
            .any(|status| matches!(*status, b'R' | b'C'))
        {
            let Some(original_path) = records.next() else {
                return BTreeSet::new();
            };
            insert_git_porcelain_path(&mut paths, original_path);
        }
    }
    paths
}

fn insert_git_porcelain_path(paths: &mut BTreeSet<String>, path: &[u8]) {
    if let Ok(path) = std::str::from_utf8(path)
        && !path.is_empty()
    {
        paths.insert(normalize_slashes(path));
    }
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
    if step.validation_receipt_ids.is_empty() {
        return false;
    }
    let validation = step
        .validation_receipt_ids
        .iter()
        .rev()
        .find_map(|receipt_id| {
            document.validation_receipts.iter().rev().find(|receipt| {
                receipt.id == *receipt_id
                    && receipt.proof_bearing
                    && receipt.tool_success
                    && receipt.epoch == document.evidence_epoch
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

fn normalized_nonempty_strings(values: BTreeSet<String>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn path_is_within_root(candidate: &Path, root: &Path) -> bool {
    repository_root_paths_equal(candidate, root) || candidate.starts_with(root)
}

fn status_from_document(document: &TaskEvidenceDocument, message: &str) -> TaskLifecycleStatus {
    let validation_receipt_ids = document
        .validation_receipts
        .iter()
        .filter(|receipt| {
            receipt.epoch == document.evidence_epoch
                && receipt.tool_success
                && receipt.proof_bearing
                && receipt.verdict.as_deref() == Some("VERIFIED")
        })
        .map(|receipt| receipt.id.clone())
        .collect();
    let command_receipt_ids = document
        .command_receipts
        .iter()
        .filter(|receipt| {
            receipt.epoch == document.evidence_epoch && receipt.exit_code == 0 && !receipt.timed_out
        })
        .map(|receipt| receipt.id.clone())
        .collect();
    TaskLifecycleStatus {
        phase: document.lifecycle.phase,
        outcome: document.lifecycle.outcome,
        mutation_revision: document.host_mutation_revision,
        accepted_evidence_revision: document.lifecycle.accepted_evidence_revision,
        review_required: document.lifecycle.review_required,
        closure_fingerprint: document.lifecycle.closure_fingerprint.clone(),
        incomplete_occurrences: document.lifecycle.incomplete_occurrences,
        validation_receipt_ids,
        command_receipt_ids,
        message: message.to_string(),
    }
}

fn closure_evidence_issues(
    document: &TaskEvidenceDocument,
    closure: &ClosureSubmission,
    gate: &TaskCompletionGate,
) -> Vec<String> {
    let mut issues = gate.reasons.clone();
    if gate.status != TaskCompletionStatus::Passed && issues.is_empty() {
        issues.push(format!(
            "runtime completion gate is {}",
            completion_status_name(gate.status)
        ));
    }
    if document.host_mutation_revision > 0 && closure.validation_receipt_ids.is_empty() {
        issues.push("closure did not cite a fresh validation receipt".to_string());
    }
    for receipt_id in &closure.validation_receipt_ids {
        let valid = document.validation_receipts.iter().any(|receipt| {
            receipt.id == *receipt_id
                && receipt.epoch == document.evidence_epoch
                && receipt.tool_success
                && receipt.proof_bearing
                && receipt.verdict.as_deref() == Some("VERIFIED")
        });
        if !valid {
            issues.push(format!(
                "validation receipt `{receipt_id}` is missing, stale, or non-passing"
            ));
        }
    }
    for receipt_id in &closure.runtime_evidence {
        let valid = document.command_receipts.iter().any(|receipt| {
            receipt.id == *receipt_id
                && receipt.epoch == document.evidence_epoch
                && receipt.exit_code == 0
                && !receipt.timed_out
        });
        if !valid {
            issues.push(format!(
                "runtime evidence receipt `{receipt_id}` is missing, stale, or unsuccessful"
            ));
        }
    }
    let owned_paths = task_owned_file_paths(document);
    if document.host_mutation_revision > 0 && closure.path_review.is_empty() {
        issues.push("closure did not include a post-mutation path review".to_string());
    }
    for owned_path in owned_paths {
        if !closure
            .path_review
            .iter()
            .any(|reviewed| path_is_covered(&owned_path, reviewed))
        {
            issues.push(format!(
                "task-controlled path `{owned_path}` is absent from the closure path review"
            ));
        }
    }
    if document
        .lifecycle
        .classification
        .as_ref()
        .is_some_and(|classification| classification.exhaustive)
        && closure.competing_paths_checked.is_empty()
    {
        issues.push(
            "exhaustive closure did not identify the competing runtime paths checked".to_string(),
        );
    }
    issues.extend(
        closure
            .missing_requirement_ids
            .iter()
            .map(|id| format!("missing requirement `{id}`")),
    );
    issues.extend(
        closure
            .blocked_reasons
            .iter()
            .map(|reason| format!("blocked: {reason}")),
    );
    issues.sort();
    issues.dedup();
    issues
}

fn verified_work_marker(document: &TaskEvidenceDocument) -> String {
    let current_validations = document
        .validation_receipts
        .iter()
        .filter(|receipt| receipt.epoch == document.evidence_epoch)
        .map(|receipt| {
            (
                receipt.id.as_str(),
                receipt.tool_success,
                receipt.proof_bearing,
                receipt.verdict.as_deref(),
                &receipt.active_files,
            )
        })
        .collect::<Vec<_>>();
    let current_commands = document
        .command_receipts
        .iter()
        .filter(|receipt| receipt.epoch == document.evidence_epoch)
        .map(|receipt| {
            (
                receipt.id.as_str(),
                receipt.exit_code,
                receipt.timed_out,
                receipt.possible_mutation,
            )
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "epoch": document.evidence_epoch,
        "mutation_revision": document.host_mutation_revision,
        "validations": current_validations,
        "commands": current_commands,
        "files": document.latest_file_hashes,
        "artifacts": document.latest_generated_artifact_hashes,
    }))
    .expect("verified work marker serialization cannot fail");
    format!("{:x}", Sha256::digest(bytes))
}

fn closure_fingerprint(
    document: &TaskEvidenceDocument,
    closure: &ClosureSubmission,
    gate: &TaskCompletionGate,
    issues: &[String],
) -> String {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "mutation_revision": document.host_mutation_revision,
        "evidence_epoch": document.evidence_epoch,
        "closure": closure,
        "gate": gate,
        "issues": issues,
    }))
    .expect("closure fingerprint serialization cannot fail");
    format!("{:x}", Sha256::digest(bytes))
}

fn lifecycle_review_required(document: &TaskEvidenceDocument) -> bool {
    let Some(classification) = document.lifecycle.classification.as_ref() else {
        return false;
    };
    if classification.exhaustive
        || classification.risk_domains.iter().any(|domain| {
            matches!(
                domain.as_str(),
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
    {
        return true;
    }
    let paths = task_owned_file_paths(document);
    paths.len() > 20
        || paths.iter().any(|path| {
            let path = path.to_ascii_lowercase();
            [
                "protocol",
                "schema",
                "sandbox",
                "approval",
                "permission",
                "task_evidence",
                "session/turn",
                "tools/registry",
                "publish",
                "install",
            ]
            .iter()
            .any(|needle| path.contains(needle))
        })
}

fn review_binding_hash(document: &TaskEvidenceDocument, gate: &TaskCompletionGate) -> String {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "thread_id": document.thread_id,
        "repository_root": document.start.repository_root,
        "mutation_revision": document.host_mutation_revision,
        "evidence_epoch": document.evidence_epoch,
        "accepted_evidence_revision": document.lifecycle.accepted_evidence_revision,
        "closure": document.lifecycle.accepted_closure,
        "gate": gate,
        "files": document.latest_file_hashes,
        "artifacts": document.latest_generated_artifact_hashes,
    }))
    .expect("review binding serialization cannot fail");
    format!("{:x}", Sha256::digest(bytes))
}

fn task_evidence_review_prompt(
    document: &TaskEvidenceDocument,
    gate: &TaskCompletionGate,
) -> String {
    let packet = serde_json::to_string_pretty(&serde_json::json!({
        "repository_root": document.start.repository_root,
        "classification": document.lifecycle.classification,
        "closure": document.lifecycle.accepted_closure,
        "completion_gate": gate,
        "changed_paths": task_owned_file_paths(document),
        "validation_receipts": document
            .validation_receipts
            .iter()
            .filter(|receipt| receipt.epoch == document.evidence_epoch)
            .map(|receipt| serde_json::json!({
                "id": receipt.id,
                "mode": receipt.mode,
                "verdict": receipt.verdict,
                "proof_bearing": receipt.proof_bearing,
                "files": receipt.active_files,
            }))
            .collect::<Vec<_>>(),
    }))
    .expect("review packet serialization cannot fail");
    format!(
        "Independently review the exact KD4 task revision below. Inspect the repository read-only, \
verify the changed runtime paths, competing paths, and cited validation evidence, and look for \
correctness or wiring defects. Return one exact JSON object matching ReviewOutputEvent. Use \
`overall_correctness` = `patch is correct` only when there are no actionable findings; otherwise \
describe every finding. Do not wrap the JSON in Markdown.\n\n{packet}"
    )
}

fn invalidate_lifecycle_for_new_work(document: &mut TaskEvidenceDocument, reason: &str) {
    document.lifecycle.phase = if document.lifecycle.classification.is_some() {
        TaskPhase::Fixing
    } else {
        TaskPhase::Unclassified
    };
    document.lifecycle.outcome = None;
    document.lifecycle.accepted_evidence_revision = None;
    document.lifecycle.accepted_closure = None;
    document.lifecycle.review_required = false;
    document.lifecycle.prepared_review_binding = None;
    document.lifecycle.clean_review_hash = None;
    document.lifecycle.closure_fingerprint = None;
    document.lifecycle.incomplete_evidence_marker = None;
    document.lifecycle.incomplete_occurrences = 0;
    document.lifecycle.message = reason.to_string();
}

fn invalidate_for_mutation(document: &mut TaskEvidenceDocument) {
    document.host_mutation_revision = document.host_mutation_revision.saturating_add(1);
    invalidate_evidence(document, true, true);
    invalidate_lifecycle_for_new_work(
        document,
        "repository mutation invalidated prior closure and review evidence",
    );
}

fn invalidate_for_plan_change(document: &mut TaskEvidenceDocument) {
    invalidate_evidence(document, false, false);
    invalidate_lifecycle_for_new_work(
        document,
        "plan scope changed and requires fresh closure evidence",
    );
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
    use tempfile::TempDir;

    async fn ledger_fixture() -> (TempDir, TaskEvidenceLedger) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(repo.join("scripts"))
            .await
            .expect("scripts");
        tokio::fs::create_dir_all(repo.join(".git"))
            .await
            .expect("git dir");
        tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
            .await
            .expect("verifier");
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
            .begin_verify_local_validation(&[])
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
            .begin_verify_local_validation(&[])
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

    #[tokio::test]
    async fn active_mutation_guard_blocks_closure_until_released() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .classify(TaskClassification::default())
            .await
            .expect("classification");
        let mutation_guard = ledger
            .guard_named_mutation("normalized_shell_mutation", "turn", false)
            .await
            .expect("mutation reservation")
            .expect("enabled mutation guard");

        let error = ledger
            .submit_closure(ClosureSubmission::default())
            .await
            .expect_err("active mutation must block closure");
        assert!(error.contains("repository mutation is active"));

        drop(mutation_guard);
        let status = ledger
            .submit_closure(ClosureSubmission::default())
            .await
            .expect("closure after mutation release");
        assert_eq!(status.phase, TaskPhase::Fixing);
    }

    #[tokio::test]
    async fn identical_incomplete_closure_without_verified_work_terminates_blocked() {
        let (_temp, ledger) = ledger_fixture().await;
        let classified = ledger
            .classify(TaskClassification::default())
            .await
            .expect("classification");
        assert_eq!(classified.phase, TaskPhase::Fixing);

        let first = ledger
            .submit_closure(ClosureSubmission::default())
            .await
            .expect("first closure");
        assert_eq!(first.phase, TaskPhase::Fixing);
        assert_eq!(first.incomplete_occurrences, 1);

        let second = ledger
            .submit_closure(ClosureSubmission::default())
            .await
            .expect("second closure");
        assert_eq!(second.phase, TaskPhase::Ready);
        assert_eq!(second.outcome, Some(TaskOutcome::Blocked));
        assert_eq!(second.incomplete_occurrences, 2);
        assert!(
            ledger
                .authorize_final_item("turn", "item")
                .await
                .expect("authorization")
        );
    }
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
