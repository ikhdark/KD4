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

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 3;
const FILE_HASH_CHUNK_SIZE: usize = 64 * 1024;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
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
    #[cfg(test)]
    persistence_test_control: Arc<std::sync::Mutex<Option<PersistenceTestControl>>>,
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
    generated_artifact_requirements: Vec<GeneratedArtifactRequirement>,
    #[serde(default)]
    latest_generated_artifact_hashes: BTreeMap<String, FileHashSnapshot>,
    latest_file_hashes: BTreeMap<String, FileHashSnapshot>,
    risks: Vec<EvidenceRisk>,
    desktop_activation_receipt: Option<DesktopActivationReceipt>,
    repair_turns_used: u8,
    #[serde(default = "initial_receipt_sequence")]
    next_edit_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_command_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_external_evidence_receipt_sequence: u64,
    #[serde(default)]
    host_mutation_revision: u64,
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
                generated_artifact_requirements: Vec::new(),
                latest_generated_artifact_hashes: BTreeMap::new(),
                latest_file_hashes: BTreeMap::new(),
                risks: storage_failure_reason
                    .as_deref()
                    .map(|reason| vec![task_evidence_storage_risk(reason, 0)])
                    .unwrap_or_default(),
                desktop_activation_receipt: None,
                repair_turns_used: 0,
                next_edit_receipt_sequence: initial_receipt_sequence(),
                next_command_receipt_sequence: initial_receipt_sequence(),
                next_external_evidence_receipt_sequence: initial_receipt_sequence(),
                host_mutation_revision: 0,
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
            codex_home: Some(codex_home.clone()),
            thread_id: Some(thread_id_text.clone()),
            evidence_path: writable_evidence_path,
            repo_root: Some(repo_root),
            document: Arc::new(Mutex::new(Some(document.clone()))),
            persistence_gate: Arc::new(Semaphore::new(1)),
            external_evidence_gate: Arc::new(Semaphore::new(1)),
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
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
                    if self.evidence_path.is_some() {
                        resolve_risk(document, "task-evidence-storage-failure");
                    }
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
        gate.status = TaskCompletionStatus::Blocked;
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
    if schema_version > u64::from(TASK_EVIDENCE_SCHEMA_VERSION) {
        return ExistingDocument::NewerSchema { schema_version };
    }
    if schema_version == 0 {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: format!("unsupported schema version {schema_version}"),
        };
    }
    let schema_version = schema_version as u32;
    let legacy_completion_model = schema_version < TASK_EVIDENCE_SCHEMA_VERSION
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
    ExistingDocument::Loaded {
        document: Box::new(document),
        legacy_completion_model,
    }
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
    let legacy_completion_model = document.schema_version < TASK_EVIDENCE_SCHEMA_VERSION;
    migrate_document_with_completion_model(document, legacy_completion_model);
}

fn migrate_document_with_completion_model(
    document: &mut TaskEvidenceDocument,
    legacy_completion_model: bool,
) {
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
    document.schema_version = TASK_EVIDENCE_SCHEMA_VERSION;
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
            .is_none_or(|receipt| receipt.epoch != document.evidence_epoch)
    {
        blocked.push("required Desktop activation receipt is missing or stale".to_string());
    }
    for requirement in &document.generated_artifact_requirements {
        if let Some(path) = requirement.path.as_ref()
            && !generated_artifact_is_currently_available(document, path)
        {
            blocked.push(format!(
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

fn invalidate_for_mutation(document: &mut TaskEvidenceDocument) {
    document.host_mutation_revision = document.host_mutation_revision.saturating_add(1);
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.last_mutation_at = Some(timestamp());
    document.desktop_activation_receipt = None;
    document.repair_turns_used = 0;
    document.completion = None;
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
        || document
            .risks
            .iter()
            .any(|risk| risk.source == "task_evidence_storage" && !risk.resolved)
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
            .record_plan_update(&plan(StepStatus::InProgress))
            .await;
        let warning = ledger.take_finalization_warning().await.expect("warning");
        assert!(warning.contains("No automatic repair turn was started"));
        assert!(ledger.take_finalization_warning().await.is_none());
    }
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
