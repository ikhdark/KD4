use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Utc;
use codex_git_utils::collect_git_info;
use codex_protocol::ThreadId;
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
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::warn;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 2;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_VALIDATION_RECEIPTS: usize = 64;
const WIRING_GUARD_PLUGIN_VERSION: &str = "0.1.16";
const WIRING_GUARD_LEDGER_SCHEMA_VERSION: &str = "1.3.0";
const WIRING_GUARD_REPORT_SCHEMA_VERSION: &str = "1.5.0";
const WIRING_GUARD_PROOF_GRAPH_SCHEMA_VERSION: &str = "1.0.0";
const WIRING_GUARD_EDITOR_SCHEMA_VERSION: &str = "1.0.0";

pub(crate) struct TaskEvidenceLedger {
    evidence_path: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    trusted_wiring_guard_root: Option<PathBuf>,
    document: Mutex<Option<TaskEvidenceDocument>>,
    persistence_gate: Semaphore,
    last_persisted_revision: AtomicU64,
    wiring_ledger_starts: Mutex<BTreeMap<String, WiringLedgerFingerprint>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WiringLedgerFingerprint {
    entry_count: usize,
    last_entry_sha1: Option<String>,
    trusted_launcher: Option<PathBuf>,
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
    wiring_receipt: Option<EpochReceipt>,
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
struct EpochReceipt {
    receipt_id: String,
    epoch: u64,
    recorded_at: String,
    #[serde(default)]
    wiring_proof: Option<WiringProof>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WiringProof {
    schema_id: String,
    schema_version: String,
    report_schema_version: String,
    timestamp: String,
    diff_hash: String,
    checked_changed_files: Vec<String>,
    proof_graph_id: String,
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
        let Some(repo_root) = find_kd4_repo_root(cwd) else {
            return Self::disabled();
        };
        let evidence_path = codex_home
            .join("task-evidence")
            .join(format!("{thread_id}.json"));
        let trusted_wiring_guard_root = find_trusted_wiring_guard_root(&codex_home);
        let now = timestamp();
        let thread_id_text = thread_id.to_string();
        let repository_root = repo_root.to_string_lossy().into_owned();

        let existing =
            load_existing_document(&evidence_path, &thread_id_text, &repository_root).await;
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
                wiring_receipt: None,
                desktop_activation_receipt: None,
                automatic_plan_attempt_epoch: None,
                repair_turns_used: 0,
                next_edit_receipt_sequence: initial_receipt_sequence(),
                next_command_receipt_sequence: initial_receipt_sequence(),
                next_validation_receipt_sequence: initial_receipt_sequence(),
                completion: None,
            }
        };
        let writable_evidence_path = storage_failure_reason.is_none().then_some(evidence_path);
        let ledger = Self {
            evidence_path: writable_evidence_path,
            repo_root: Some(repo_root),
            trusted_wiring_guard_root,
            document: Mutex::new(Some(document.clone())),
            persistence_gate: Semaphore::new(1),
            last_persisted_revision: AtomicU64::new(0),
            wiring_ledger_starts: Mutex::new(BTreeMap::new()),
        };
        if storage_failure_reason.is_none() {
            let _ = ledger.persist_document(&document).await;
        }
        ledger
    }

    pub(crate) fn disabled() -> Self {
        Self {
            evidence_path: None,
            repo_root: None,
            trusted_wiring_guard_root: None,
            document: Mutex::new(None),
            persistence_gate: Semaphore::new(1),
            last_persisted_revision: AtomicU64::new(0),
            wiring_ledger_starts: Mutex::new(BTreeMap::new()),
        }
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

    pub(crate) async fn record_plan_update(&self, update: &UpdatePlanArgs) -> UpdatePlanArgs {
        let Some(_) = self.repo_root else {
            return update.clone();
        };
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

    pub(crate) async fn record_command_intent(&self, call_id: &str, command: &[String]) {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let Some(trusted_launcher) = trusted_wiring_guard_check_invocation(
            command,
            self.trusted_wiring_guard_root.as_deref(),
        ) else {
            self.wiring_ledger_starts.lock().await.remove(call_id);
            return;
        };
        let Some(mut fingerprint) = wiring_ledger_fingerprint(repo_root).await else {
            self.wiring_ledger_starts.lock().await.remove(call_id);
            return;
        };
        fingerprint.trusted_launcher = Some(trusted_launcher);
        self.wiring_ledger_starts
            .lock()
            .await
            .insert(call_id.to_string(), fingerprint);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command(
        &self,
        call_id: &str,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
    ) {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let wiring_ledger_start = self.wiring_ledger_starts.lock().await.remove(call_id);
        let command_succeeded = exit_code == 0 && !timed_out;
        let trusted_launcher = trusted_wiring_guard_check_invocation(
            command,
            self.trusted_wiring_guard_root.as_deref(),
        );
        let wiring_proof = if command_succeeded {
            if let Some(before) = wiring_ledger_start.as_ref() {
                if before.trusted_launcher.as_ref() == trusted_launcher.as_ref() {
                    read_fresh_wiring_proof(repo_root, duration_ms, before).await
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
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
                    id: receipt_id.clone(),
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
                if let Some(wiring_proof) = wiring_proof {
                    document.wiring_receipt = Some(EpochReceipt {
                        receipt_id,
                        epoch: document.evidence_epoch,
                        recorded_at: timestamp(),
                        wiring_proof: Some(wiring_proof),
                    });
                    promote_steps_with_fresh_evidence(document);
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
        let bytes = match serde_json::to_vec_pretty(document) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("failed to serialize KD4 task evidence: {err}");
                return PersistOutcome::Failed;
            }
        };
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

enum ExistingDocument {
    Missing,
    Loaded(Box<TaskEvidenceDocument>),
    Rejected { kind: &'static str, reason: String },
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
    if document.start.repository_root != expected_repository_root {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: "repository root does not match the requested checkout".to_string(),
        };
    }
    ExistingDocument::Loaded(Box::new(document))
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
    let (duplicate_command_indices, duplicate_command_ids) = duplicate_receipt_indices(
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
    if document
        .wiring_receipt
        .as_ref()
        .is_some_and(|receipt| duplicate_command_ids.contains(&receipt.receipt_id))
    {
        document.wiring_receipt = None;
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
    if step_requires_wiring(step) {
        let Some(receipt) = document.wiring_receipt.as_ref() else {
            return false;
        };
        let Some(proof) = receipt.wiring_proof.as_ref() else {
            return false;
        };
        if receipt.epoch != document.evidence_epoch || !wiring_proof_covers_step(proof, step) {
            return false;
        }
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
    if document.plan.iter().any(|step| {
        step_requires_wiring(step)
            && document.wiring_receipt.as_ref().is_none_or(|receipt| {
                receipt.epoch != document.evidence_epoch
                    || receipt
                        .wiring_proof
                        .as_ref()
                        .is_none_or(|proof| !wiring_proof_covers_step(proof, step))
            })
    }) {
        partial.push(
            "structured static wiring proof is missing, stale, or out of scope for changed code"
                .to_string(),
        );
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

fn invalidate_for_mutation(document: &mut TaskEvidenceDocument) {
    invalidate_evidence(document, true, true);
}

fn invalidate_for_plan_change(document: &mut TaskEvidenceDocument) {
    invalidate_evidence(document, false, false);
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
    document.wiring_receipt = None;
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

fn step_requires_wiring(step: &EvidencePlanStep) -> bool {
    step.edit_paths.iter().any(|path| is_code_path(path))
        || step.runtime_paths.iter().any(|path| is_code_path(path))
}

fn is_code_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "rs" | "py"
                    | "js"
                    | "jsx"
                    | "ts"
                    | "tsx"
                    | "go"
                    | "java"
                    | "kt"
                    | "c"
                    | "cc"
                    | "cpp"
                    | "h"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "swift"
            )
        })
}

fn find_trusted_wiring_guard_root(codex_home: &Path) -> Option<PathBuf> {
    let relative_root = Path::new("plugins")
        .join("cache")
        .join("local-wiring-guards")
        .join("wiring-guard")
        .join(WIRING_GUARD_PLUGIN_VERSION);
    let mut candidates = vec![codex_home.join(&relative_root)];
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        candidates.push(parent.join(relative_root));
    }
    candidates
        .into_iter()
        .find_map(|candidate| validate_trusted_wiring_guard_root(&candidate))
}

fn validate_trusted_wiring_guard_root(candidate: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(candidate).ok()?;
    let manifest = serde_json::from_slice::<Value>(
        &std::fs::read(canonical.join("bundle-manifest.json")).ok()?,
    )
    .ok()?;
    if manifest.get("schema_id")?.as_str()? != "wiring-guard/bundle-manifest"
        || manifest.get("schema_version")?.as_str()? != "1.0.0"
        || manifest.pointer("/plugin/name")?.as_str()? != "wiring-guard"
        || manifest.pointer("/plugin/version")?.as_str()? != WIRING_GUARD_PLUGIN_VERSION
    {
        return None;
    }
    let ledger_schema = serde_json::from_slice::<Value>(
        &std::fs::read(canonical.join("schemas").join("ledger.schema.json")).ok()?,
    )
    .ok()?;
    if ledger_schema
        .pointer("/$defs/entry/properties/schema_version/const")?
        .as_str()?
        != WIRING_GUARD_LEDGER_SCHEMA_VERSION
        || ledger_schema
            .pointer("/$defs/entry/properties/report_schema_version/const")?
            .as_str()?
            != WIRING_GUARD_REPORT_SCHEMA_VERSION
    {
        return None;
    }
    Some(canonical)
}

fn trusted_wiring_guard_check_invocation(
    command: &[String],
    trusted_root: Option<&Path>,
) -> Option<PathBuf> {
    let trusted_root = trusted_root?;
    if command
        .iter()
        .any(|argument| argument.contains('\r') || argument.contains('\n'))
    {
        return None;
    }
    let words = command
        .iter()
        .flat_map(|argument| argument.split_whitespace())
        .map(|word| {
            word.trim_matches(|character| matches!(character, '\'' | '"'))
                .to_string()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let executable_index = words.iter().position(|word| {
        let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
        matches!(
            name.to_ascii_lowercase().as_str(),
            "wiring_guard.py" | "wiring_guard.cmd" | "wiring_guard.sh"
        )
    })?;
    if words.iter().enumerate().any(|(index, word)| {
        if word == "&" {
            return index + 1 != executable_index;
        }
        word.chars()
            .any(|character| matches!(character, '&' | ';' | '|' | '>' | '<' | '`'))
            || word.contains("$(")
    }) {
        return None;
    }
    if !wiring_guard_prefix_executes(&words[..executable_index]) {
        return None;
    }
    let launcher =
        validate_trusted_wiring_guard_launcher(Path::new(&words[executable_index]), trusted_root)?;
    let arguments = &words[executable_index + 1..];
    let check_index = arguments
        .iter()
        .position(|word| word.eq_ignore_ascii_case("check"))?;
    arguments[check_index + 1..]
        .iter()
        .any(|word| word.eq_ignore_ascii_case("--ledger"))
        .then_some(launcher)
}

fn validate_trusted_wiring_guard_launcher(path: &Path, trusted_root: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    if canonical.parent()? != trusted_root.join("runtime") {
        return None;
    }
    let file_name = canonical.file_name()?.to_str()?;
    if !matches!(
        file_name.to_ascii_lowercase().as_str(),
        "wiring_guard.py" | "wiring_guard.cmd" | "wiring_guard.sh"
    ) {
        return None;
    }
    let relative_path = format!("runtime/{}", file_name.replace('\\', "/"));
    let manifest = serde_json::from_slice::<Value>(
        &std::fs::read(trusted_root.join("bundle-manifest.json")).ok()?,
    )
    .ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    let declared = manifest
        .get("files")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("path").and_then(Value::as_str) == Some(&relative_path))?;
    let digest = declared.get("sha256")?.as_str()?;
    let launcher_bytes = std::fs::read(&canonical).ok()?;
    if declared.get("size")?.as_u64()? != metadata.len()
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || sha256_hex(&launcher_bytes) != digest
    {
        return None;
    }
    Some(canonical)
}

fn wiring_guard_prefix_executes(prefix: &[String]) -> bool {
    if prefix.is_empty() {
        return true;
    }
    prefix.iter().all(|word| {
        let normalized = word.to_ascii_lowercase();
        let name = normalized.rsplit(['/', '\\']).next().unwrap_or(&normalized);
        matches!(
            name,
            "python"
                | "python.exe"
                | "python3"
                | "python3.exe"
                | "py"
                | "py.exe"
                | "bash"
                | "bash.exe"
                | "sh"
                | "sh.exe"
                | "pwsh"
                | "pwsh.exe"
                | "powershell"
                | "powershell.exe"
                | "cmd"
                | "cmd.exe"
                | "env"
                | "call"
                | "-c"
                | "/c"
                | "--command"
                | "-command"
                | "-noprofile"
                | "-noninteractive"
                | "-executionpolicy"
                | "bypass"
                | "-u"
                | "--"
                | "&"
        )
    })
}

async fn wiring_ledger_fingerprint(repo_root: &Path) -> Option<WiringLedgerFingerprint> {
    let ledger_path = wiring_guard_ledger_path(repo_root).await?;
    let bytes = match tokio::fs::read(ledger_path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Some(WiringLedgerFingerprint::default());
        }
        Err(_) => return None,
    };
    let entries = serde_json::from_slice::<Vec<Value>>(&bytes).ok()?;
    fingerprint_wiring_entries(&entries)
}

fn fingerprint_wiring_entries(entries: &[Value]) -> Option<WiringLedgerFingerprint> {
    let last_entry_sha1 = entries
        .last()
        .map(serde_json::to_vec)
        .transpose()
        .ok()?
        .map(|bytes| sha1_hex(&bytes));
    Some(WiringLedgerFingerprint {
        entry_count: entries.len(),
        last_entry_sha1,
        trusted_launcher: None,
    })
}

async fn read_fresh_wiring_proof(
    repo_root: &Path,
    duration_ms: u64,
    before: &WiringLedgerFingerprint,
) -> Option<WiringProof> {
    let ledger_path = wiring_guard_ledger_path(repo_root).await?;
    let bytes = tokio::fs::read(ledger_path).await.ok()?;
    let entries = serde_json::from_slice::<Vec<Value>>(&bytes).ok()?;
    let after = fingerprint_wiring_entries(&entries)?;
    if after.entry_count <= before.entry_count || after.last_entry_sha1 == before.last_entry_sha1 {
        return None;
    }
    let entry = entries.last()?.as_object()?;
    let schema_id = entry.get("schema_id")?.as_str()?;
    let schema_version = entry.get("schema_version")?.as_str()?;
    let report_schema_version = entry.get("report_schema_version")?.as_str()?;
    let timestamp = entry.get("timestamp")?.as_str()?;
    let diff_hash = entry.get("diff_hash")?.as_str()?;
    if schema_id != "wiring-guard/ledger-entry"
        || schema_version != WIRING_GUARD_LEDGER_SCHEMA_VERSION
        || report_schema_version != WIRING_GUARD_REPORT_SCHEMA_VERSION
        || entry.get("verdict")?.as_str()? != "WIRED"
        || !is_lower_hex_id(diff_hash, 64, "")
        || !matches!(entry.get("mode")?.as_str()?, "summary" | "full")
        || !valid_wiring_findings(entry.get("findings")?)
        || !json_object_array(entry.get("normalized_findings")?)?.is_empty()
        || !valid_runtime_evidence(entry.get("runtime_evidence")?)
        || !entry.get("finding_policy")?.is_object()
        || !valid_suggested_fixes(entry.get("suggested_fixes")?)
    {
        return None;
    }
    let changed_files = json_string_array(entry.get("changed_files")?)?;
    let checked_changed_files = json_string_array(entry.get("checked_changed_files")?)?;
    if checked_changed_files.is_empty()
        || checked_changed_files
            .iter()
            .any(|checked| !changed_files.contains(checked))
    {
        return None;
    }
    let proof_graph = entry.get("proof_graph")?.as_object()?;
    let proof_graph_id = proof_graph.get("graph_id")?.as_str()?;
    if proof_graph.get("schema_id")?.as_str()? != "wiring-guard/proof-graph"
        || proof_graph.get("schema_version")?.as_str()? != WIRING_GUARD_PROOF_GRAPH_SCHEMA_VERSION
        || !is_lower_hex_id(proof_graph_id, 24, "PG-")
        || json_object_array(proof_graph.get("nodes")?)?.is_empty()
        || json_object_array(proof_graph.get("edges")?).is_none()
        || !valid_wiring_traces(proof_graph.get("traces")?)
        || proof_graph
            .get("summary")?
            .as_object()?
            .get("open_findings")?
            .as_u64()?
            != 0
        || proof_graph
            .get("verdict")
            .and_then(Value::as_str)
            .is_some_and(|verdict| verdict != "WIRED")
    {
        return None;
    }
    let editor = entry.get("editor")?.as_object()?;
    if editor.get("schema_id")?.as_str()? != "wiring-guard/editor"
        || editor.get("schema_version")?.as_str()? != WIRING_GUARD_EDITOR_SCHEMA_VERSION
        || editor.get("graph_id")?.as_str()? != proof_graph_id
        || json_object_array(editor.get("diagnostics")?).is_none()
        || json_object_array(editor.get("code_lenses")?).is_none()
    {
        return None;
    }
    let recorded_at = DateTime::parse_from_rfc3339(timestamp)
        .ok()?
        .with_timezone(&Utc);
    let now = Utc::now();
    let bounded_duration_ms = duration_ms.min(24 * 60 * 60 * 1_000);
    let earliest = now
        - ChronoDuration::milliseconds(i64::try_from(bounded_duration_ms).ok()?)
        - ChronoDuration::seconds(2);
    if recorded_at < earliest || recorded_at > now + ChronoDuration::seconds(2) {
        return None;
    }
    Some(WiringProof {
        schema_id: schema_id.to_string(),
        schema_version: schema_version.to_string(),
        report_schema_version: report_schema_version.to_string(),
        timestamp: timestamp.to_string(),
        diff_hash: diff_hash.to_string(),
        checked_changed_files: checked_changed_files
            .into_iter()
            .map(|path| normalize_slashes(&path))
            .collect(),
        proof_graph_id: proof_graph_id.to_string(),
    })
}

fn json_object_array(value: &Value) -> Option<&Vec<Value>> {
    let values = value.as_array()?;
    values.iter().all(Value::is_object).then_some(values)
}

fn is_lower_hex_id(value: &str, hex_length: usize, prefix: &str) -> bool {
    value.len() == prefix.len() + hex_length
        && value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_wiring_traces(value: &Value) -> bool {
    value.as_array().is_some_and(|traces| {
        traces.iter().all(|trace| {
            let Some(trace) = trace.as_object() else {
                return false;
            };
            trace
                .get("finding_id")
                .and_then(Value::as_str)
                .is_some_and(|id| is_lower_hex_id(id, 16, "WG-"))
                && trace
                    .get("locations")
                    .and_then(Value::as_array)
                    .is_some_and(|locations| {
                        locations.iter().all(|location| {
                            let Some(location) = location.as_object() else {
                                return false;
                            };
                            location
                                .get("file")
                                .and_then(Value::as_str)
                                .is_some_and(|path| !path.is_empty())
                                && location
                                    .get("line")
                                    .and_then(Value::as_u64)
                                    .is_some_and(|line| line > 0)
                        })
                    })
        })
    })
}

fn valid_wiring_findings(value: &Value) -> bool {
    let Some(findings) = value.as_object() else {
        return false;
    };
    let connected = |name: &str| {
        findings
            .get(name)
            .and_then(json_object_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .all(|entry| entry.get("status").and_then(Value::as_str) == Some("connected"))
            })
    };
    connected("must_reach")
        && connected("runtime_contracts")
        && [
            "deleted_callers",
            "orphans",
            "stubs",
            "bad_code",
            "stale_arms",
            "inconclusive",
        ]
        .iter()
        .all(|name| {
            findings
                .get(*name)
                .and_then(json_object_array)
                .is_some_and(std::vec::Vec::is_empty)
        })
        && findings
            .get("replaces")
            .and_then(json_object_array)
            .is_some()
}

fn valid_runtime_evidence(value: &Value) -> bool {
    value.as_array().is_some_and(|entries| {
        entries.iter().all(|entry| {
            let Some(entry) = entry.as_object() else {
                return false;
            };
            entry.get("schema_version").and_then(Value::as_str) == Some("1.0.0")
                && entry
                    .get("evidence_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| is_lower_hex_id(id, 24, "runtime-evidence-"))
                && entry
                    .get("provider_id")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && entry
                    .get("provider_version")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && entry
                    .get("tool_version")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                && entry.get("contract_id").is_some_and(json_string_or_null)
                && entry
                    .get("diff_hash")
                    .and_then(Value::as_str)
                    .is_some_and(|hash| is_lower_hex_id(hash, 64, ""))
                && entry
                    .get("command")
                    .and_then(Value::as_array)
                    .is_some_and(|command| {
                        !command.is_empty() && command.iter().all(Value::is_string)
                    })
                && entry.get("working_directory").is_some_and(Value::is_string)
                && entry
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| {
                        matches!(status, "connected" | "missing" | "inconclusive" | "error")
                    })
                && entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .is_some_and(|timestamp| DateTime::parse_from_rfc3339(timestamp).is_ok())
                && entry
                    .get("duration_ms")
                    .and_then(Value::as_f64)
                    .is_some_and(|duration| duration >= 0.0)
                && entry
                    .get("exit_code")
                    .is_some_and(|value| value.is_null() || value.as_i64().is_some())
                && entry.get("execution_policy").is_some_and(Value::is_object)
                && entry.get("reference").is_none_or(json_string_or_null)
                && entry.get("reason").is_none_or(json_string_or_null)
                && entry.get("stdout_sha256").is_none_or(|value| {
                    value.is_null()
                        || value
                            .as_str()
                            .is_some_and(|hash| is_lower_hex_id(hash, 64, ""))
                })
        })
    })
}

fn json_string_or_null(value: &Value) -> bool {
    value.is_string() || value.is_null()
}

fn valid_suggested_fixes(value: &Value) -> bool {
    value.as_array().is_some_and(|fixes| {
        fixes.iter().all(|fix| {
            let Some(fix) = fix.as_object() else {
                return false;
            };
            fix.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| is_lower_hex_id(id, 16, "PGX-"))
                && fix
                    .get("finding_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| is_lower_hex_id(id, 16, "WG-"))
                && fix
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|title| !title.is_empty())
                && fix.get("kind").and_then(Value::as_str) == Some("command")
                && fix
                    .get("command")
                    .and_then(Value::as_array)
                    .is_some_and(|command| {
                        !command.is_empty() && command.iter().all(Value::is_string)
                    })
                && fix
                    .get("safe_to_apply_automatically")
                    .and_then(Value::as_bool)
                    == Some(false)
        })
    })
}

async fn wiring_guard_ledger_path(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    let git_dir = match tokio::fs::metadata(&dot_git).await {
        Ok(metadata) if metadata.is_dir() => Some(dot_git),
        Ok(_) => tokio::fs::read_to_string(&dot_git)
            .await
            .ok()
            .and_then(|contents| {
                contents
                    .trim()
                    .strip_prefix("gitdir:")
                    .map(str::trim)
                    .map(PathBuf::from)
            })
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    repo_root.join(path)
                }
            }),
        Err(_) => None,
    };
    if let Some(git_dir) = git_dir {
        return Some(
            git_dir
                .join("codex")
                .join("wiring-guard")
                .join("ledger.json"),
        );
    }
    Some(
        repo_root
            .join(".codex")
            .join("wiring-guard")
            .join("ledger.json"),
    )
}

fn json_string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect()
}

fn wiring_proof_covers_step(proof: &WiringProof, step: &EvidencePlanStep) -> bool {
    let mut paths = step
        .edit_paths
        .iter()
        .filter(|path| is_code_path(path))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        paths.extend(step.runtime_paths.iter().filter(|path| is_code_path(path)));
    }
    !paths.is_empty()
        && paths.iter().all(|path| {
            proof
                .checked_changed_files
                .iter()
                .any(|checked| path_is_covered(path, checked))
        })
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

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

    async fn install_wiring_guard_fixture(codex_home: &Path) -> PathBuf {
        let root = codex_home
            .join("plugins/cache/local-wiring-guards/wiring-guard")
            .join(WIRING_GUARD_PLUGIN_VERSION);
        tokio::fs::create_dir_all(root.join("runtime"))
            .await
            .expect("wiring runtime");
        tokio::fs::create_dir_all(root.join("schemas"))
            .await
            .expect("wiring schemas");
        let launcher = root.join("runtime/wiring_guard.py");
        let launcher_bytes = b"# trusted wiring guard fixture\n";
        tokio::fs::write(&launcher, launcher_bytes)
            .await
            .expect("wiring launcher");
        tokio::fs::write(
            root.join("bundle-manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_id": "wiring-guard/bundle-manifest",
                "schema_version": "1.0.0",
                "plugin": {"name": "wiring-guard", "version": WIRING_GUARD_PLUGIN_VERSION},
                "files": [{
                    "path": "runtime/wiring_guard.py",
                    "sha256": sha256_hex(launcher_bytes),
                    "size": launcher_bytes.len()
                }]
            }))
            .expect("wiring manifest json"),
        )
        .await
        .expect("wiring manifest");
        tokio::fs::write(
            root.join("schemas/ledger.schema.json"),
            serde_json::to_vec(&serde_json::json!({
                "$defs": {"entry": {"properties": {
                    "schema_version": {"const": WIRING_GUARD_LEDGER_SCHEMA_VERSION},
                    "report_schema_version": {"const": WIRING_GUARD_REPORT_SCHEMA_VERSION}
                }}}
            }))
            .expect("wiring schema json"),
        )
        .await
        .expect("wiring schema");
        launcher
    }

    async fn ledger_fixture() -> (TempDir, TaskEvidenceLedger) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let codex_home = temp.path().join("home");
        install_wiring_guard_fixture(&codex_home).await;
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

    fn wiring_ledger_entry(path: &str) -> Value {
        let graph_id = format!("PG-{}", "b".repeat(24));
        serde_json::json!({
            "schema_id": "wiring-guard/ledger-entry",
            "schema_version": WIRING_GUARD_LEDGER_SCHEMA_VERSION,
            "report_schema_version": WIRING_GUARD_REPORT_SCHEMA_VERSION,
            "timestamp": timestamp(),
            "verdict": "WIRED",
            "diff_hash": "a".repeat(64),
            "changed_files": [path],
            "checked_changed_files": [path],
            "mode": "summary",
            "findings": {
                "must_reach": [{"status": "connected"}],
                "runtime_contracts": [],
                "replaces": [],
                "deleted_callers": [],
                "orphans": [],
                "stubs": [],
                "bad_code": [],
                "stale_arms": [],
                "inconclusive": []
            },
            "normalized_findings": [],
            "runtime_evidence": [],
            "finding_policy": {},
            "proof_graph": {
                "schema_id": "wiring-guard/proof-graph",
                "schema_version": WIRING_GUARD_PROOF_GRAPH_SCHEMA_VERSION,
                "graph_id": graph_id,
                "verdict": "WIRED",
                "nodes": [{}],
                "edges": [],
                "traces": [],
                "summary": {"open_findings": 0}
            },
            "suggested_fixes": [],
            "editor": {
                "schema_id": "wiring-guard/editor",
                "schema_version": WIRING_GUARD_EDITOR_SCHEMA_VERSION,
                "graph_id": graph_id,
                "diagnostics": [],
                "code_lenses": []
            }
        })
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
    async fn minimal_or_legacy_wiring_ledger_entry_is_rejected() {
        let (temp, _ledger) = ledger_fixture().await;
        let repo = temp.path().join("repo");
        let before = wiring_ledger_fingerprint(&repo)
            .await
            .expect("initial fingerprint");
        let ledger_path = wiring_guard_ledger_path(&repo).await.expect("ledger path");
        tokio::fs::create_dir_all(ledger_path.parent().expect("ledger parent"))
            .await
            .expect("ledger parent");
        tokio::fs::write(
            &ledger_path,
            serde_json::to_vec(&vec![serde_json::json!({
                "schema_id": "wiring-guard/ledger-entry",
                "schema_version": "1.0.0",
                "report_schema_version": "1.0.0",
                "timestamp": timestamp(),
                "verdict": "WIRED",
                "diff_hash": "a".repeat(64),
                "checked_changed_files": ["src/lib.rs"],
                "findings": {},
                "normalized_findings": [],
                "runtime_evidence": [],
                "proof_graph": {
                    "schema_id": "wiring-guard/proof-graph",
                    "schema_version": "1.0.0",
                    "graph_id": format!("PG-{}", "b".repeat(24))
                }
            })])
            .expect("legacy ledger json"),
        )
        .await
        .expect("legacy ledger");

        assert!(read_fresh_wiring_proof(&repo, 1, &before).await.is_none());
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
        let cwd_uri = PathUri::from_abs_path(&cwd);
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
        let wiring_launcher = temp
            .path()
            .join("home/plugins/cache/local-wiring-guards/wiring-guard")
            .join(WIRING_GUARD_PLUGIN_VERSION)
            .join("runtime/wiring_guard.py");
        let wiring_command = [
            "python".to_string(),
            wiring_launcher.to_string_lossy().into_owned(),
            "check".to_string(),
            "--ledger".to_string(),
        ];
        ledger
            .record_command_intent("wiring-1", &wiring_command)
            .await;
        let ledger_path = repo
            .join(".git")
            .join("codex")
            .join("wiring-guard")
            .join("ledger.json");
        tokio::fs::create_dir_all(ledger_path.parent().expect("ledger parent"))
            .await
            .expect("ledger parent");
        tokio::fs::write(
            &ledger_path,
            serde_json::to_vec(&vec![wiring_ledger_entry("src/lib.rs")]).expect("serialize ledger"),
        )
        .await
        .expect("write ledger");
        ledger
            .record_command("wiring-1", &wiring_command, &cwd_uri, 0, false, 10, false)
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
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
