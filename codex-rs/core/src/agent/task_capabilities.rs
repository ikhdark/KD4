use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::CapabilityProfile;
use codex_agent_task_store::repository_lineage_id;
use codex_agent_task_store::repository_workspace_id;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

/// Coarse capability class used before dispatching a typed-agent tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TypedToolClass {
    AgentCommunication,
    OwnTask,
    RootTaskControl,
    ReadSearch,
    CodeModeControl,
    Shell,
    StructuredEdit,
    DynamicExternal,
    Unknown,
}

impl TypedToolClass {
    /// Static catalog eligibility, not an execution grant. Assignment-specific
    /// checks (including patch scope and task operations) still run at dispatch.
    pub(crate) fn available_to_subagents(self) -> bool {
        matches!(
            self,
            Self::AgentCommunication
                | Self::OwnTask
                | Self::ReadSearch
                | Self::CodeModeControl
                | Self::Shell
                | Self::StructuredEdit
        )
    }

    /// Discovery labels describe the existing class, never grant authority.
    pub(crate) fn native_capability_group(self) -> Option<&'static str> {
        match self {
            Self::AgentCommunication => Some("Native agent communication"),
            Self::OwnTask => Some("Native task state"),
            Self::RootTaskControl => Some("Native task control"),
            Self::ReadSearch => Some("Native read/search"),
            Self::CodeModeControl => Some("Native orchestration"),
            Self::Shell => Some("Native shell/process execution"),
            Self::StructuredEdit => Some("Native structured edits"),
            Self::DynamicExternal | Self::Unknown => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum CapabilityPolicyError {
    #[error("assignment repository {expected} does not match active repository {actual}")]
    RepositoryMismatch { expected: String, actual: String },
    #[error("assignment workspace {expected} does not match active workspace {actual}")]
    WorkspaceMismatch { expected: String, actual: String },
    #[error("assignment role {role:?} requires capability profile {expected:?}, got {actual:?}")]
    RoleProfileMismatch {
        role: codex_agent_task_store::AgentRole,
        expected: CapabilityProfile,
        actual: CapabilityProfile,
    },
    #[error("repository root {path:?} cannot be canonicalized: {reason}")]
    InvalidRepositoryRoot { path: String, reason: String },
    #[error("invalid repository-relative path {path:?}: {reason}")]
    InvalidRepoPath { path: String, reason: String },
    #[error("cold-review mutation evidence belongs to assignment {actual}, expected {expected}")]
    ColdReviewAssignmentMismatch {
        expected: AssignmentId,
        actual: AssignmentId,
    },
    #[error("cold-review mutation evidence belongs to attempt {actual}, expected {expected}")]
    ColdReviewAttemptMismatch {
        expected: AttemptId,
        actual: AttemptId,
    },
}

/// Closed cold-review payload. This type intentionally has no worker-reasoning or conversation-
/// history fields, so callers cannot accidentally forward either through normal construction or
/// serialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ColdReviewContext {
    pub assignment: Assignment,
    pub effective_contract: codex_agent_task_store::EffectiveTaskContract,
    pub attempt_id: AttemptId,
    /// None means no current receipt, not a declaration of zero changed paths.
    pub target_receipt_scope: Option<ColdReviewReceiptScope>,
    pub applicable_instructions: Vec<String>,
    pub attempt_specific_diff: String,
    /// Compatibility projection: automatic mutation snapshots are disabled.
    /// An empty array is not evidence that the worker changed no paths.
    pub observed_writes: [(); 0],
    pub relevant_contracts: Vec<String>,
    pub nearest_tests: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ColdReviewReceiptScope {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub status: codex_agent_task_store::AgentStatusClaim,
    pub declared_paths: Vec<String>,
    pub provenance: &'static str,
    pub freshness: &'static str,
}

pub(crate) fn cold_review_receipt_scope(
    task: &codex_agent_task_store::AgentTask,
) -> Result<Option<ColdReviewReceiptScope>, CapabilityPolicyError> {
    let Some(receipt) = task.receipt.as_ref() else {
        return Ok(None);
    };
    if receipt.assignment_id != task.assignment.assignment_id {
        return Err(CapabilityPolicyError::ColdReviewAssignmentMismatch {
            expected: task.assignment.assignment_id,
            actual: receipt.assignment_id,
        });
    }
    if receipt.attempt_id != task.current_attempt.attempt_id {
        return Err(CapabilityPolicyError::ColdReviewAttemptMismatch {
            expected: task.current_attempt.attempt_id,
            actual: receipt.attempt_id,
        });
    }
    let mut paths = Vec::with_capacity(receipt.declared_changes.len());
    for change in &receipt.declared_changes {
        normalize_repo_path_lexically(&change.path)?;
        // Stored declarations are already normalized. Do not resolve old paths
        // against today's filesystem or copy worker-authored summaries.
        paths.push(change.path.clone());
    }
    Ok(Some(ColdReviewReceiptScope {
        assignment_id: receipt.assignment_id,
        attempt_id: receipt.attempt_id,
        status: receipt.status,
        declared_paths: paths,
        provenance: "worker_declared",
        freshness: "unverified",
    }))
}

/// Inputs are limited to independently recoverable review evidence. Worker reasoning and
/// conversation history are not accepted by this API.
#[derive(Clone, Debug)]
pub(crate) struct ColdReviewContextInput {
    pub assignment: Assignment,
    pub amendment: Option<codex_agent_task_store::AttemptAmendment>,
    pub attempt_id: AttemptId,
    pub applicable_instructions: Vec<String>,
    pub attempt_specific_diff: String,
    pub relevant_contracts: Vec<String>,
    pub nearest_tests: Vec<String>,
}

pub(crate) fn build_cold_review_context(
    repo_root: &Path,
    input: ColdReviewContextInput,
) -> Result<ColdReviewContext, CapabilityPolicyError> {
    verify_assignment_authority(&input.assignment, repo_root)?;
    canonical_repository_root(repo_root)?;

    Ok(ColdReviewContext {
        effective_contract: input
            .assignment
            .effective_contract(input.amendment.as_ref()),
        assignment: input.assignment,
        attempt_id: input.attempt_id,
        target_receipt_scope: None,
        applicable_instructions: input.applicable_instructions,
        attempt_specific_diff: input.attempt_specific_diff,
        observed_writes: [],
        relevant_contracts: input.relevant_contracts,
        nearest_tests: input.nearest_tests,
    })
}

fn normalize_repo_path_lexically(path: &str) -> Result<String, CapabilityPolicyError> {
    if path.trim().is_empty() {
        return Err(invalid_path(path, "path is empty"));
    }
    let normalized_separators = path.replace('\\', "/");
    if normalized_separators.starts_with('/')
        || normalized_separators
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':')
    {
        return Err(invalid_path(path, "absolute paths are not allowed"));
    }
    let mut components = Vec::new();
    for component in normalized_separators.split('/') {
        match component {
            "" => return Err(invalid_path(path, "empty components are not allowed")),
            "." => return Err(invalid_path(path, "dot components are not allowed")),
            ".." => return Err(invalid_path(path, "path traversal is not allowed")),
            value if value.contains('\0') => {
                return Err(invalid_path(path, "NUL bytes are not allowed"));
            }
            value => components.push(value),
        }
    }
    if components.is_empty() {
        return Err(invalid_path(path, "path is empty"));
    }
    Ok(components.join("/"))
}

fn verify_assignment_authority(
    assignment: &Assignment,
    repo_root: &Path,
) -> Result<(), CapabilityPolicyError> {
    let expected = assignment.role.capability_profile();
    if assignment.capability_profile != expected {
        return Err(CapabilityPolicyError::RoleProfileMismatch {
            role: assignment.role,
            expected,
            actual: assignment.capability_profile,
        });
    }
    let actual = repository_identity(repo_root)?;
    if assignment.repository_id != actual {
        return Err(CapabilityPolicyError::RepositoryMismatch {
            expected: assignment.repository_id.clone(),
            actual,
        });
    }
    let actual_workspace = repository_workspace_id(repo_root).map_err(|error| {
        CapabilityPolicyError::InvalidRepositoryRoot {
            path: repo_root.to_string_lossy().into_owned(),
            reason: error.to_string(),
        }
    })?;
    if assignment.workspace_id != actual_workspace {
        return Err(CapabilityPolicyError::WorkspaceMismatch {
            expected: assignment.workspace_id.clone(),
            actual: actual_workspace,
        });
    }
    Ok(())
}

fn repository_identity(repo_root: &Path) -> Result<String, CapabilityPolicyError> {
    repository_lineage_id(repo_root).map_err(|error| CapabilityPolicyError::InvalidRepositoryRoot {
        path: repo_root.to_string_lossy().into_owned(),
        reason: error.to_string(),
    })
}

fn canonical_repository_root(repo_root: &Path) -> Result<PathBuf, CapabilityPolicyError> {
    std::fs::canonicalize(repo_root).map_err(|error| CapabilityPolicyError::InvalidRepositoryRoot {
        path: repo_root.to_string_lossy().into_owned(),
        reason: error.to_string(),
    })
}

/// This turn's capability refusals. A refusal leaves the turn running, so the
/// terminal boundary must still say whether permitted work could have replaced the
/// refused operation. The host records what it observed and never reads intent from prose.
#[derive(Default)]
pub(crate) struct CapabilityRefusals(std::sync::Mutex<Vec<CapabilityRefusal>>);

struct CapabilityRefusal {
    tool: String,
    /// Reads can stand in for refused inspection, never for a refused source edit.
    replaceable: bool,
    followed_by_work: bool,
}

impl CapabilityRefusals {
    fn entries(&self) -> std::sync::MutexGuard<'_, Vec<CapabilityRefusal>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn record(&self, tool: &str, class: TypedToolClass) {
        let mut entries = self.entries();
        match entries.iter_mut().find(|refusal| refusal.tool == tool) {
            Some(refusal) => refusal.followed_by_work = false,
            None => entries.push(CapabilityRefusal {
                tool: tool.to_string(),
                replaceable: class != TypedToolClass::StructuredEdit,
                followed_by_work: false,
            }),
        }
    }

    pub(crate) fn record_permitted_work(&self) {
        for refusal in self.entries().iter_mut() {
            refusal.followed_by_work = true;
        }
    }

    /// Unresolved refusals make the host's disposition `incomplete`; a replaced
    /// one stays an advisory because the worker may have legitimately finished.
    pub(crate) fn apply_to(
        &self,
        assessment: &mut codex_protocol::protocol::TurnCompletionAssessment,
    ) {
        for refusal in self.entries().iter() {
            let tool = &refusal.tool;
            if refusal.replaceable && refusal.followed_by_work {
                assessment.advisories.push(format!(
                    "Refused `{tool}`; permitted work followed, but the report may not cover the refused operation."
                ));
                continue;
            }
            assessment.verification_gaps.push(if refusal.replaceable {
                format!("Refused `{tool}` and no permitted work followed, so the report may not establish the assignment.")
            } else {
                format!("Refused `{tool}`; this assignment has no permitted route to that work, so the report may not establish it.")
            });
            let disposition = assessment.task_disposition.get_or_insert_with(|| {
                codex_protocol::protocol::TurnTaskDisposition {
                    schema_version: 1,
                    plan_revision: None,
                    input_revision: None,
                    mutation_revision: 0,
                    request_state: String::new(),
                    artifact_state: "semantic_acceptance_unverified".into(),
                    terminal_action: String::new(),
                    unmet_obligations: Vec::new(),
                    evidence_refs: Vec::new(),
                    authority: "host_capability_refusal; not independent task acceptance".into(),
                }
            });
            disposition.request_state = "incomplete".into();
            disposition.terminal_action = "handoff_with_gaps".into();
            disposition.evidence_refs.push(format!("native:capability_refusal:{tool}"));
        }
    }
}

/// One refused assignment tool call. Its only conversion is a no-effects
/// `PermissionDenied` failure, so a refusal cannot end the assignment; terminal
/// outcomes stay with the router's `RequiredOperationBlocked` paths.
#[derive(Debug)]
pub(crate) struct CapabilityDenied {
    semantic_name: String,
    reason: String,
}

impl From<CapabilityDenied> for crate::FunctionCallError {
    fn from(denied: CapabilityDenied) -> Self {
        let CapabilityDenied { semantic_name, reason } = denied;
        let mut failure = codex_protocol::tool_failure::ToolFailure::no_effects(
            codex_protocol::tool_failure::ToolFailureCode::PermissionDenied,
            format!("{semantic_name}: {reason}"),
        );
        failure.details = Some(serde_json::json!({"denial_kind":"capability_denied"}));
        failure.safe_next_actions.push(
            "Continue the assignment using native read/search or already-authorized scoped apply_patch; return the available evidence or a precise blocker. Do not retry the prohibited action through another route.".into(),
        );
        Self::structured(failure)
    }
}

/// Classes come from host registration, not task prose. Worker commands inherit
/// turn permissions; native source edits use the scope-checked patch owner.
/// A refusal denies this call only: the worker keeps its turn, so it can use a
/// permitted route or report the blocker instead of losing its completed work.
pub(crate) fn authorize_assignment_tool(
    authorization: Option<&codex_agent_task_store::AgentTaskAuthorization>,
    class: TypedToolClass,
    semantic_name: &str,
    payload: &crate::tools::context::ToolPayload,
) -> Result<(), CapabilityDenied> {

    let refused = |reason: &str| CapabilityDenied {
        semantic_name: semantic_name.to_string(),
        reason: reason.to_string(),
    };
    if matches!(semantic_name, "spawn_agent" | "followup_task") {
        return Err(refused("workers cannot delegate execution authority; ask the root to admit the assignment"));
    }
    match class {
        TypedToolClass::ReadSearch | TypedToolClass::Shell | TypedToolClass::AgentCommunication
        | TypedToolClass::CodeModeControl => Ok(()),
        TypedToolClass::OwnTask => {
            let grouped_gate = semantic_name == "agent_task" && match payload {
                crate::tools::context::ToolPayload::Function { arguments, .. } =>
                    serde_json::from_str::<serde_json::Value>(arguments).ok()
                        .and_then(|value| value.get("operation").and_then(|value| value.as_str()).map(str::to_owned))
                        .is_some_and(|operation| operation == "set_gate"),
                _ => true,
            };
            if semantic_name == "set_agent_gate" || grouped_gate {
                Err(refused("workers cannot seal acceptance gates; the root must independently review their receipts"))
            } else { Ok(()) }
        }
        TypedToolClass::StructuredEdit if semantic_name == "apply_patch" => {
            let authorization = authorization.ok_or_else(||
                refused("unbound subagents are read-only; admit an explicit typed writable assignment"))?;
            codex_agent_task_store::require_write_contract(&authorization.assignment,
                authorization.current_attempt.amendment.as_ref())
                .map_err(|error| refused(&error.to_string()))?;
            codex_agent_task_store::dispatch_manifest(&authorization.assignment,
                authorization.current_attempt.amendment.as_ref())
                .map_err(|error| refused(&error.to_string()))?;
            if authorization.assignment.write_scope.is_empty() {
                return Err(refused("message-only/read-only assignments cannot write source"));
            }
            Ok(())
        }
        TypedToolClass::RootTaskControl => Err(refused(
            "subagents cannot create or broaden execution contracts; ask the root to admit work explicitly")),
        _ => Err(refused(
            "this external/IO tool cannot enforce the worker's write contract; use native read/search, commands within inherited permissions and task scope, or scoped apply_patch, with root-owned integration")),
    }
}

fn invalid_path(path: &str, reason: impl Into<String>) -> CapabilityPolicyError {
    CapabilityPolicyError::InvalidRepoPath {
        path: path.to_string(),
        reason: reason.into(),
    }
}

