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
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use tokio::sync::Mutex;
use tracing::warn;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 1;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_VALIDATION_RECEIPTS: usize = 64;

pub(crate) struct TaskEvidenceLedger {
    evidence_path: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    document: Mutex<Option<TaskEvidenceDocument>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskEvidenceDocument {
    schema_version: u32,
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
    latest_file_hashes: BTreeMap<String, FileHashSnapshot>,
    risks: Vec<EvidenceRisk>,
    verify_plan_epoch: Option<u64>,
    validation_epoch: Option<u64>,
    wiring_receipt: Option<EpochReceipt>,
    desktop_activation_receipt: Option<DesktopActivationReceipt>,
    #[serde(default)]
    automatic_plan_attempt_epoch: Option<u64>,
    repair_turns_used: u8,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileHashSnapshot {
    path: String,
    sha1: Option<String>,
    exists: bool,
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
        let now = timestamp();
        let thread_id_text = thread_id.to_string();
        let repository_root = repo_root.to_string_lossy().into_owned();

        let existing = match tokio::fs::read(&evidence_path).await {
            Ok(bytes) => serde_json::from_slice::<TaskEvidenceDocument>(&bytes)
                .ok()
                .filter(|document| {
                    document.schema_version == TASK_EVIDENCE_SCHEMA_VERSION
                        && document.thread_id == thread_id_text
                        && document.start.repository_root == repository_root
                }),
            Err(_) => None,
        };
        let document = if let Some(mut document) = existing {
            document.updated_at = now;
            document
        } else {
            let git = collect_git_info(&repo_root).await;
            TaskEvidenceDocument {
                schema_version: TASK_EVIDENCE_SCHEMA_VERSION,
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
                latest_file_hashes: BTreeMap::new(),
                risks: Vec::new(),
                verify_plan_epoch: None,
                validation_epoch: None,
                wiring_receipt: None,
                desktop_activation_receipt: None,
                automatic_plan_attempt_epoch: None,
                repair_turns_used: 0,
                completion: None,
            }
        };
        let ledger = Self {
            evidence_path: Some(evidence_path),
            repo_root: Some(repo_root),
            document: Mutex::new(Some(document.clone())),
        };
        ledger.persist_document(&document).await;
        ledger
    }

    pub(crate) fn disabled() -> Self {
        Self {
            evidence_path: None,
            repo_root: None,
            document: Mutex::new(None),
        }
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
                for (index, item) in update.plan.iter().enumerate() {
                    let id = effective_step_id(item, index, &mut used_ids);
                    let old = previous.get(&id);
                    let status =
                        normalize_requested_status(&item.status, old.map(|step| &step.status));
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
                        edit_paths: old.map_or_else(BTreeSet::new, |step| step.edit_paths.clone()),
                        validation_receipt_ids: old
                            .map_or_else(Vec::new, |step| step.validation_receipt_ids.clone()),
                    });
                }
                let plan_changed = normalized != document.plan;
                document.plan = normalized;
                document.active_step_id = document
                    .plan
                    .iter()
                    .find(|step| step.status == StepStatus::InProgress)
                    .map(|step| step.id.clone());
                rebuild_declared_requirements_and_risks(document);
                promote_steps_with_fresh_evidence(document);
                if plan_changed {
                    document.repair_turns_used = 0;
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
            if before != &after {
                transitions.push(FileHashTransition {
                    path: before.path.clone(),
                    before_sha1: before.sha1.clone(),
                    after_sha1: after.sha1.clone(),
                    before_exists: before.exists,
                    after_exists: after.exists,
                });
            }
            after_snapshots.push(after);
        }

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
                    let mut affected_steps = BTreeSet::new();
                    if let Some(step_id) = intent.step_id.as_ref() {
                        affected_steps.insert(step_id.clone());
                    }
                    for transition in &transitions {
                        for step in &document.plan {
                            if step.edit_paths.contains(&transition.path) {
                                affected_steps.insert(step.id.clone());
                            }
                        }
                    }
                    for step in &mut document.plan {
                        if affected_steps.contains(&step.id) {
                            for transition in &transitions {
                                step.edit_paths.insert(transition.path.clone());
                            }
                            if !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped) {
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
                        document
                            .latest_file_hashes
                            .insert(after.path.clone(), after);
                    }
                    document.edit_receipts.push(EditReceipt {
                        id: format!("edit-{}", document.edit_receipts.len() + 1),
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
                let receipt_id = format!("command-{}", document.command_receipts.len() + 1);
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
                if is_wiring_guard_check(command) && exit_code == 0 && !timed_out {
                    document.wiring_receipt = Some(EpochReceipt {
                        receipt_id,
                        epoch: document.evidence_epoch,
                        recorded_at: timestamp(),
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
        active_files: &[PathBuf],
        stale_reasons: &[String],
        payload: Option<&Value>,
    ) {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let normalized_active_files = active_files
            .iter()
            .map(|path| normalize_input_path(repo_root, Some(repo_root), path))
            .collect::<Vec<_>>();
        let mut file_snapshots = Vec::with_capacity(normalized_active_files.len());
        for path in &normalized_active_files {
            file_snapshots.push(snapshot_file(repo_root, path).await);
        }
        let declared_artifacts = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| {
                    document
                        .generated_artifact_requirements
                        .iter()
                        .filter_map(|requirement| requirement.path.clone())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default()
        };
        let mut artifact_snapshots = Vec::new();
        for path in declared_artifacts {
            artifact_snapshots.push(snapshot_file(repo_root, &path).await);
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                let receipt_id = format!("validation-{}", document.validation_receipts.len() + 1);
                document.validation_receipts.push(ValidationReceipt {
                    id: receipt_id.clone(),
                    recorded_at: timestamp(),
                    epoch: document.evidence_epoch,
                    step_id: document.active_step_id.clone(),
                    mode: mode.to_string(),
                    verdict: verdict.map(str::to_string),
                    tool_success,
                    proof_bearing,
                    active_files: file_snapshots,
                    stale_reasons: stale_reasons.to_vec(),
                    payload: payload.cloned(),
                });
                trim_to_last(&mut document.validation_receipts, MAX_VALIDATION_RECEIPTS);

                if mode == "plan" && tool_success {
                    document.verify_plan_epoch = Some(document.evidence_epoch);
                    rebuild_verifier_requirements(document, payload);
                }
                if proof_bearing {
                    document.validation_epoch = Some(document.evidence_epoch);
                    for snapshot in artifact_snapshots {
                        document
                            .generated_artifact_hashes
                            .insert(snapshot.path.clone(), snapshot);
                    }
                    for step in &mut document.plan {
                        if step.edit_paths.iter().all(|path| {
                            normalized_active_files
                                .iter()
                                .any(|active| path_is_covered(path, active))
                        }) {
                            step.validation_receipt_ids.push(receipt_id.clone());
                        }
                    }
                    resolve_risks_by_source(document, "verify_local");
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
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    pub(crate) async fn take_finalization_repair_prompt(&self) -> Option<String> {
        let gate = self.completion_gate().await?;
        if gate.status == TaskCompletionStatus::Passed {
            return None;
        }
        let (changed_paths, snapshot) = self
            .update_document(|document| {
                if document.repair_turns_used >= 1 {
                    return None;
                }
                document.repair_turns_used += 1;
                document.updated_at = timestamp();
                Some(
                    document
                        .latest_file_hashes
                        .keys()
                        .take(20)
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
            .await?;
        let changed_paths = changed_paths?;
        self.persist_document(&snapshot).await;

        let reasons = gate.reasons.iter().take(4).cloned().collect::<Vec<_>>();
        let reasons = format!(
            "{} Changed paths: [{}]",
            reasons.join("; "),
            changed_paths.join(", ")
        );
        Some(format!(
            "KD4 task-evidence finalization is {status}. This is the single bounded evidence-repair continuation. If an automatic read-only verify_local plan result is included above, inspect it; otherwise call plan mode with the changed paths. Address the plan, then call verify_local in `fast` or `final` mode for a proof-bearing JSON receipt. If code changed, also run the declared Wiring Guard check; if Desktop activation is required, obtain its runtime receipt. Do not claim completion unless the resulting machine gate is passed. Current reasons: {reasons}.",
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
        self.persist_document(&snapshot).await;
        Some(gate)
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
        let expected = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| document.latest_file_hashes.clone())
                .unwrap_or_default()
        };
        if expected.is_empty() {
            return;
        }
        let mut changed = Vec::new();
        for (path, previous) in expected {
            let current = snapshot_file(repo_root, &path).await;
            if current != previous {
                changed.push(current);
            }
        }
        if changed.is_empty() {
            return;
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                invalidate_for_mutation(document);
                for current in changed {
                    let path = current.path.clone();
                    document.latest_file_hashes.insert(path.clone(), current);
                    for step in &mut document.plan {
                        if step.edit_paths.contains(&path)
                            && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                        {
                            step.status = StepStatus::Implemented;
                            step.validation_receipt_ids.clear();
                        }
                    }
                }
                upsert_risk(
                    document,
                    EvidenceRisk {
                        id: format!("external-change-{}", document.evidence_epoch),
                        description: "a task-controlled file changed after its recorded evidence"
                            .to_string(),
                        source: "freshness".to_string(),
                        blocking: false,
                        resolved: false,
                        epoch: document.evidence_epoch,
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
        Some((result, document.clone()))
    }

    async fn persist_document(&self, document: &TaskEvidenceDocument) {
        let Some(path) = self.evidence_path.as_ref() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let bytes = match serde_json::to_vec_pretty(document) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("failed to serialize KD4 task evidence: {err}");
                return;
            }
        };
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            warn!("failed to create KD4 task-evidence directory: {err}");
            return;
        }
        let temp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::now_v7()));
        if let Err(err) = tokio::fs::write(&temp, bytes).await {
            warn!("failed to write KD4 task-evidence temp file: {err}");
            return;
        }
        if let Err(first_err) = tokio::fs::rename(&temp, path).await {
            let _ = tokio::fs::remove_file(path).await;
            if let Err(err) = tokio::fs::rename(&temp, path).await {
                let _ = tokio::fs::remove_file(&temp).await;
                warn!("failed to persist KD4 task evidence ({first_err}; retry: {err})");
            }
        }
    }
}

fn find_kd4_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|candidate| {
            candidate.join("scripts").join("verify_local.py").is_file()
                && candidate.join("kd4_features.toml").is_file()
        })
        .map(Path::to_path_buf)
}

fn effective_step_id(item: &PlanItemArg, index: usize, used_ids: &mut BTreeSet<String>) -> String {
    if let Some(id) = item.id.as_ref() {
        used_ids.insert(id.clone());
        return id.clone();
    }
    let digest = sha1_hex(item.step.trim().as_bytes());
    let base = format!("step-{}", &digest[..12]);
    if used_ids.insert(base.clone()) {
        return base;
    }
    let fallback = format!("{base}-{}", index + 1);
    used_ids.insert(fallback.clone());
    fallback
}

fn normalize_requested_status(requested: &StepStatus, previous: Option<&StepStatus>) -> StepStatus {
    match requested {
        StepStatus::Passed | StepStatus::Completed => {
            if previous == Some(&StepStatus::Passed) {
                StepStatus::Passed
            } else {
                StepStatus::Implemented
            }
        }
        status => status.clone(),
    }
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
            });
    }
}

fn promote_steps_with_fresh_evidence(document: &mut TaskEvidenceDocument) {
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
    let validation = document
        .validation_receipts
        .iter()
        .rev()
        .find(|receipt| receipt.proof_bearing && receipt.epoch == document.evidence_epoch);
    let Some(validation) = validation else {
        return false;
    };
    if step.edit_paths.iter().any(|path| {
        !validation
            .active_files
            .iter()
            .any(|active| path_is_covered(path, &active.path))
    }) {
        return false;
    }
    if step_requires_wiring(step)
        && document
            .wiring_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.epoch != document.evidence_epoch)
    {
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
        if document
            .generated_artifact_hashes
            .get(&normalized)
            .is_none_or(|snapshot| !snapshot.exists)
        {
            return false;
        }
    }
    true
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
        .filter(|step| !matches!(step.status, StepStatus::Passed | StepStatus::Skipped))
        .map(|step| format!("{} ({:?})", step.id, step.status))
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
    if document.plan.iter().any(step_requires_wiring)
        && document
            .wiring_receipt
            .as_ref()
            .is_none_or(|receipt| receipt.epoch != document.evidence_epoch)
    {
        partial.push("static wiring proof is missing or stale for changed code".to_string());
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
        if let Some(path) = requirement.path.as_ref()
            && document
                .generated_artifact_hashes
                .get(path)
                .is_none_or(|snapshot| !snapshot.exists)
        {
            blocked.push(format!("required generated artifact is missing: {path}"));
        }
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
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.last_mutation_at = Some(timestamp());
    document.verify_plan_epoch = None;
    document.validation_epoch = None;
    document.wiring_receipt = None;
    document.desktop_activation_receipt = None;
    document.automatic_plan_attempt_epoch = None;
    document.repair_turns_used = 0;
    document.completion = None;
    for step in &mut document.plan {
        if step.status == StepStatus::Passed {
            step.status = StepStatus::Implemented;
        }
        step.validation_receipt_ids.clear();
    }
}

fn task_is_tracked(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        || !document.edit_receipts.is_empty()
        || document
            .command_receipts
            .iter()
            .any(|receipt| receipt.possible_mutation)
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

fn is_wiring_guard_check(command: &[String]) -> bool {
    let command = command.join(" ").to_ascii_lowercase();
    command.contains("wiring_guard.py")
        && command.split_whitespace().any(|part| part == "check")
        && command.contains("--ledger")
}

fn resolve_risks_by_source(document: &mut TaskEvidenceDocument, source: &str) {
    for risk in &mut document.risks {
        if risk.source == source {
            risk.resolved = true;
        }
    }
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
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: None,
            exists: false,
        },
        Err(_) => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: None,
            exists: absolute.exists(),
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
    use tempfile::TempDir;

    async fn ledger_fixture() -> (TempDir, TaskEvidenceLedger) {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        tokio::fs::create_dir_all(repo.join("scripts"))
            .await
            .expect("scripts");
        tokio::fs::write(repo.join("scripts/verify_local.py"), "# fixture")
            .await
            .expect("verifier");
        tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
            .await
            .expect("manifest");
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
        let ledger = TaskEvidenceLedger::load_or_new(
            temp.path().join("home"),
            ThreadId::new(),
            cwd.as_path(),
        )
        .await;
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
        let cwd_uri = PathUri::from_abs_path(&cwd);
        ledger
            .record_edit_intent("patch-1", cwd.as_path(), &[PathBuf::from("src/lib.rs")])
            .await;
        tokio::fs::write(repo.join("src/lib.rs"), "pub fn value() -> u8 { 2 }")
            .await
            .expect("source update");
        ledger.record_edit_result("patch-1", "completed").await;
        ledger
            .record_verify_local(
                "plan",
                Some("PLANNED"),
                true,
                false,
                &[PathBuf::from("src/lib.rs")],
                &[],
                Some(&serde_json::json!({"planned": []})),
            )
            .await;
        ledger
            .record_verify_local(
                "final",
                Some("VERIFIED"),
                true,
                true,
                &[PathBuf::from("src/lib.rs")],
                &[],
                Some(&serde_json::json!({"verdict": "VERIFIED"})),
            )
            .await;
        ledger
            .record_command(
                &[
                    "python".to_string(),
                    "wiring_guard.py".to_string(),
                    "check".to_string(),
                    "--ledger".to_string(),
                ],
                &cwd_uri,
                0,
                false,
                10,
                false,
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
    async fn finalization_repair_is_bounded_to_one_continuation() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;
        assert!(ledger.take_finalization_repair_prompt().await.is_some());
        assert!(ledger.take_finalization_repair_prompt().await.is_none());
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
