use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::AttributionConfidence;
use codex_agent_task_store::CapabilityProfile;
use codex_agent_task_store::MutationEvidence;
use codex_agent_task_store::repository_lineage_id;
use codex_agent_task_store::repository_workspace_id;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

/// Coarse capability class used before dispatching a typed-agent tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// Whether an external tool is proven read-only or might mutate state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalMutationIntent {
    ProvenReadOnly,
    MayMutate,
}

pub(crate) fn is_independent_review_source(source: &SessionSource) -> bool {
    match source {
        SessionSource::SubAgent(SubAgentSource::Review) => true,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_role: Some(role),
            ..
        }) => role == "reviewer",
        _ => false,
    }
}

pub(crate) fn validate_independent_review_shell(
    source: &SessionSource,
    inspection_command: bool,
    sandbox_override: bool,
    additional_permissions: bool,
) -> Result<(), &'static str> {
    if !is_independent_review_source(source) {
        return Ok(());
    }
    if sandbox_override || additional_permissions {
        return Err(
            "independent reviewers cannot request shell sandbox overrides or additional permissions",
        );
    }
    if !inspection_command {
        return Err(
            "independent reviewers may run only shell commands proven read-only; on PowerShell use safelisted read-only commands (Get-Content, Get-ChildItem, Select-String, Test-Path, `rg`, `git -c core.fsmonitor=false diff --no-ext-diff --no-textconv --`, `git --no-optional-locks -c core.fsmonitor=false status --short`), without assignments, script blocks or redirection; `git log`/`show` are not safelisted, so use the supplied immutable comparison command for commit reviews",
        );
    }
    Ok(())
}

pub(crate) fn validate_independent_review_stdin(
    source: &SessionSource,
    chars: &str,
    terminate: bool,
) -> Result<(), &'static str> {
    if is_independent_review_source(source) && (!chars.is_empty() || terminate) {
        return Err("independent reviewers may only poll read-only shell commands");
    }
    Ok(())
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
    #[error("cold-review mutation evidence contains duplicate path {0:?}")]
    DuplicateColdReviewWritePath(String),
}

/// The only mutation evidence exposed to a cold reviewer. Event chronology and worker-authored
/// prose are deliberately absent; the assignment and attempt bind each path/hash tuple.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ColdReviewWriteEvidence {
    pub path: String,
    pub pre_write_hash: Option<String>,
    pub pre_write_existed: bool,
    pub final_hash: Option<String>,
    pub final_write_existed: Option<bool>,
    pub attribution_confidence: AttributionConfidence,
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
    pub observed_writes: Vec<ColdReviewWriteEvidence>,
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
    pub observed_writes: Vec<MutationEvidence>,
    pub relevant_contracts: Vec<String>,
    pub nearest_tests: Vec<String>,
}

pub(crate) fn build_cold_review_context(
    repo_root: &Path,
    input: ColdReviewContextInput,
) -> Result<ColdReviewContext, CapabilityPolicyError> {
    verify_assignment_authority(&input.assignment, repo_root)?;
    let canonical_root = canonical_repository_root(repo_root)?;
    let mut seen_paths = BTreeSet::new();
    let mut observed_writes = Vec::with_capacity(input.observed_writes.len());
    for evidence in input.observed_writes {
        if evidence.assignment_id != input.assignment.assignment_id {
            return Err(CapabilityPolicyError::ColdReviewAssignmentMismatch {
                expected: input.assignment.assignment_id,
                actual: evidence.assignment_id,
            });
        }
        if evidence.attempt_id != input.attempt_id {
            return Err(CapabilityPolicyError::ColdReviewAttemptMismatch {
                expected: input.attempt_id,
                actual: evidence.attempt_id,
            });
        }
        let path = normalize_repo_relative_path(&canonical_root, &evidence.path)?;
        if !seen_paths.insert(normalized_comparison_path(&path)) {
            return Err(CapabilityPolicyError::DuplicateColdReviewWritePath(path));
        }
        observed_writes.push(ColdReviewWriteEvidence {
            path,
            pre_write_hash: evidence.pre_write_hash,
            pre_write_existed: evidence.pre_write_existed,
            final_hash: evidence.final_hash,
            final_write_existed: evidence.final_write_existed,
            attribution_confidence: evidence.attribution_confidence,
        });
    }
    observed_writes.sort_by(|left, right| compare_repo_paths(&left.path, &right.path));

    Ok(ColdReviewContext {
        effective_contract: input
            .assignment
            .effective_contract(input.amendment.as_ref()),
        assignment: input.assignment,
        attempt_id: input.attempt_id,
        target_receipt_scope: None,
        applicable_instructions: input.applicable_instructions,
        attempt_specific_diff: input.attempt_specific_diff,
        observed_writes,
        relevant_contracts: input.relevant_contracts,
        nearest_tests: input.nearest_tests,
    })
}

// The caller canonicalizes the root once for this operation.
fn normalize_repo_relative_path(
    canonical_root: &Path,
    path: &str,
) -> Result<String, CapabilityPolicyError> {
    let normalized = normalize_repo_path_lexically(path)?;
    ensure_canonical_containment(canonical_root, &canonical_root.join(&normalized), path)?;
    Ok(normalized)
}

/// Converts one absolute local path into the repository-relative identity used by typed-task
/// scopes. Resolve existing ancestors through the filesystem before comparing native path
/// components, so case aliases and symlinks cannot redirect evidence to a different repository.
pub(crate) fn normalize_absolute_repo_path(
    repo_root: &Path,
    path: &Path,
) -> Result<String, CapabilityPolicyError> {
    if !repo_root.is_absolute() || !path.is_absolute() {
        return Err(invalid_path(
            &path.to_string_lossy(),
            "repository root and target must both be absolute",
        ));
    }

    let canonical_root = canonical_repository_root(repo_root)?;
    let target = ensure_canonical_containment(&canonical_root, path, &path.to_string_lossy())?;
    let relative = target.strip_prefix(&canonical_root).map_err(|_| {
        invalid_path(
            &path.to_string_lossy(),
            "absolute path is outside the assignment repository",
        )
    })?;
    let relative = relative.to_str().ok_or_else(|| {
        invalid_path(
            &path.to_string_lossy(),
            "path cannot be represented losslessly as UTF-8",
        )
    })?;
    normalize_repo_path_lexically(relative)
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

fn ensure_canonical_containment(
    canonical_root: &Path,
    target: &Path,
    original_path: &str,
) -> Result<PathBuf, CapabilityPolicyError> {
    let mut existing = target;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(|| {
                    invalid_path(original_path, "path has no existing repository ancestor")
                })?;
            }
            Err(error) => {
                return Err(invalid_path(
                    original_path,
                    format!("path ancestor cannot be inspected: {error}"),
                ));
            }
        }
    }
    let canonical_existing = std::fs::canonicalize(existing).map_err(|error| {
        invalid_path(
            original_path,
            format!("existing ancestor cannot be canonicalized: {error}"),
        )
    })?;
    if !canonical_existing.starts_with(canonical_root) {
        return Err(invalid_path(
            original_path,
            "path resolves outside the repository through a symlink",
        ));
    }
    let suffix = target
        .strip_prefix(existing)
        .map_err(|_| invalid_path(original_path, "invalid path ancestor"))?;
    Ok(canonical_existing.join(suffix))
}

fn compare_repo_paths(left: &str, right: &str) -> Ordering {
    normalized_comparison_path(left)
        .cmp(&normalized_comparison_path(right))
        .then_with(|| left.cmp(right))
}

fn normalized_comparison_path(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

fn invalid_path(path: &str, reason: impl Into<String>) -> CapabilityPolicyError {
    CapabilityPolicyError::InvalidRepoPath {
        path: path.to_string(),
        reason: reason.into(),
    }
}

