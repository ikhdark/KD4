use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::AttributionConfidence;
use codex_agent_task_store::CapabilityProfile;
use codex_agent_task_store::MutationEvidence;
use codex_agent_task_store::RepoScope;
use codex_agent_task_store::RiskDomain;
use codex_agent_task_store::RiskFacts;
use codex_agent_task_store::RiskGateDecision;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
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
    Diff,
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct TypedToolRequest<'a> {
    pub class: TypedToolClass,
    pub external_mutation_intent: ExternalMutationIntent,
    pub repo_paths: &'a [String],
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum CapabilityPolicyError {
    #[error("tool class {class:?} is not available to capability profile {profile:?}")]
    ToolDenied {
        profile: CapabilityProfile,
        class: TypedToolClass,
    },
    #[error("root task-control tools are not available to typed subagents")]
    RootTaskControlDenied,
    #[error("dynamic, MCP, and extension tools may not mutate for typed agents")]
    ExternalMutationDenied,
    #[error("unknown tools are denied for typed agents")]
    UnknownToolDenied,
    #[error("assignment repository {expected} does not match active repository {actual}")]
    RepositoryMismatch { expected: String, actual: String },
    #[error("assignment role {role:?} requires capability profile {expected:?}, got {actual:?}")]
    RoleProfileMismatch {
        role: codex_agent_task_store::AgentRole,
        expected: CapabilityProfile,
        actual: CapabilityProfile,
    },
    #[error("repository root {path:?} cannot be canonicalized: {reason}")]
    InvalidRepositoryRoot { path: String, reason: String },
    #[error("structured edits require at least one repository-relative path")]
    MissingStructuredEditPaths,
    #[error("invalid repository-relative path {path:?}: {reason}")]
    InvalidRepoPath { path: String, reason: String },
    #[error("structured edit path {0:?} is outside the assignment write scope")]
    PathOutsideWriteScope(String),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthorizedToolCall {
    /// Normalized paths are populated only for structured edits.
    pub normalized_repo_paths: Vec<String>,
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
    pub attempt_id: AttemptId,
    pub applicable_instructions: Vec<String>,
    pub attempt_specific_diff: String,
    pub observed_writes: Vec<ColdReviewWriteEvidence>,
    pub relevant_contracts: Vec<String>,
    pub nearest_tests: Vec<String>,
}

/// Inputs are limited to independently recoverable review evidence. Worker reasoning and
/// conversation history are not accepted by this API.
#[derive(Clone, Debug)]
pub(crate) struct ColdReviewContextInput {
    pub assignment: Assignment,
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
        let path = normalize_repo_relative_path(repo_root, &evidence.path)?;
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
        assignment: input.assignment,
        attempt_id: input.attempt_id,
        applicable_instructions: input.applicable_instructions,
        attempt_specific_diff: input.attempt_specific_diff,
        observed_writes,
        relevant_contracts: input.relevant_contracts,
        nearest_tests: input.nearest_tests,
    })
}

/// Classifies a tool name without trusting an arbitrary namespace to impersonate a core tool.
///
/// MultiAgentV2's collaboration namespace is configurable, so callers must pass the namespace
/// selected for the active turn. Any other namespace is external. When collaboration tools are
/// unnamespaced, pass `None` for both namespace arguments.
pub(crate) fn classify_typed_tool(
    namespace: Option<&str>,
    name: &str,
    collaboration_namespace: Option<&str>,
) -> TypedToolClass {
    if let Some(namespace) = namespace {
        if namespace.is_empty() || collaboration_namespace != Some(namespace) {
            return TypedToolClass::DynamicExternal;
        }
        return classify_collaboration_tool(name);
    }

    if collaboration_namespace.is_none() {
        let collaboration_class = classify_collaboration_tool(name);
        if collaboration_class != TypedToolClass::Unknown {
            return collaboration_class;
        }
    }
    if matches_name(
        name,
        &[
            "search_source",
            "read_file_span",
            "tool_search",
            "view_image",
            "list_mcp_resources",
            "list_mcp_resource_templates",
            "read_mcp_resource",
            "get_context_remaining",
            "current_time",
        ],
    ) {
        return TypedToolClass::ReadSearch;
    }
    if matches_name(name, &["git_diff"]) {
        return TypedToolClass::Diff;
    }
    if matches_name(
        name,
        &[
            "shell_command",
            "exec_command",
            "write_stdin",
            "verify_local",
        ],
    ) {
        return TypedToolClass::Shell;
    }
    if name == "apply_patch" {
        return TypedToolClass::StructuredEdit;
    }
    if starts_with_external_prefix(name) {
        return TypedToolClass::DynamicExternal;
    }
    TypedToolClass::Unknown
}

/// Applies the typed capability profile to a classified tool call.
///
/// Shell authorization is intentionally coarse here. The shell handler admits only commands that
/// the shared command-safety classifier proves read-only, plus the bounded `verify_local` tool.
/// Source mutation is authorized only for structured edits whose complete path set is in scope.
pub(crate) fn authorize_typed_tool(
    assignment: &Assignment,
    repo_root: &Path,
    request: TypedToolRequest<'_>,
) -> Result<AuthorizedToolCall, CapabilityPolicyError> {
    verify_assignment_authority(assignment, repo_root)?;
    let profile = assignment.capability_profile;
    match request.class {
        TypedToolClass::AgentCommunication
        | TypedToolClass::OwnTask
        | TypedToolClass::ReadSearch => Ok(empty_authorization()),
        TypedToolClass::RootTaskControl => Err(CapabilityPolicyError::RootTaskControlDenied),
        TypedToolClass::Diff if profile_allows_diff(profile) => Ok(empty_authorization()),
        TypedToolClass::Shell if profile_allows_shell(profile) => Ok(empty_authorization()),
        TypedToolClass::StructuredEdit => {
            authorize_structured_edit(assignment, repo_root, request.repo_paths)
        }
        TypedToolClass::DynamicExternal => {
            if request.external_mutation_intent == ExternalMutationIntent::MayMutate {
                Err(CapabilityPolicyError::ExternalMutationDenied)
            } else {
                Ok(empty_authorization())
            }
        }
        TypedToolClass::Unknown => Err(CapabilityPolicyError::UnknownToolDenied),
        class => Err(CapabilityPolicyError::ToolDenied { profile, class }),
    }
}

/// Returns whether a verifier may write a build/cache path through the central shell sandbox.
/// Invalid, absolute, traversing, cross-repository, or symlink-escaping paths fail closed.
pub(crate) fn verifier_can_write_path(
    assignment: &Assignment,
    repo_root: &Path,
    verifier_writable_roots: &[RepoScope],
    path: &str,
) -> Result<bool, CapabilityPolicyError> {
    if assignment.capability_profile != CapabilityProfile::ReadSearchShell {
        verify_assignment_authority(assignment, repo_root)?;
        return Ok(false);
    }
    typed_agent_can_write_path(assignment, repo_root, verifier_writable_roots, path)
}

/// Intersects typed-agent write authority with normalized repository paths. Source writers are
/// limited to their immutable assignment scope; verifiers are limited to centrally supplied
/// build/cache roots; explorer and reviewer profiles can never write.
pub(crate) fn typed_agent_can_write_path(
    assignment: &Assignment,
    repo_root: &Path,
    verifier_writable_roots: &[RepoScope],
    path: &str,
) -> Result<bool, CapabilityPolicyError> {
    verify_assignment_authority(assignment, repo_root)?;
    let normalized = normalize_repo_relative_path(repo_root, path)?;
    match assignment.capability_profile {
        CapabilityProfile::ReadSearch | CapabilityProfile::ReadSearchDiff => Ok(false),
        CapabilityProfile::ReadSearchShell => {
            scopes_cover_path(repo_root, verifier_writable_roots, &normalized)
        }
        CapabilityProfile::ScopedSourceWrite | CapabilityProfile::IntegratorSourceWrite => {
            scopes_cover_path(repo_root, &assignment.write_scope, &normalized)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RiskPolicyInput<'a> {
    pub changed_paths: &'a [String],
    pub configured_high_risk_paths: &'a [RepoScope],
    pub touched_contracts: &'a [String],
    pub configured_high_risk_contracts: &'a [String],
    pub cross_owner_scope: bool,
    pub named_domains: &'a [RiskDomain],
    pub non_generated_changed_files: u32,
    pub non_generated_changed_lines: u32,
    pub focused_validation_succeeded: bool,
    pub ownership_conflict: bool,
    pub drift: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivedRiskPolicy {
    pub facts: RiskFacts,
    pub decision: RiskGateDecision,
    pub matched_high_risk_path: bool,
    pub matched_high_risk_contract: bool,
}

/// Derives deterministic risk-gate facts. Runtime wiring must supply facts from stored mutation,
/// validation, ownership, and drift evidence rather than from an agent's prose claim.
pub(crate) fn derive_risk_policy(
    assignment: &Assignment,
    repo_root: &Path,
    input: RiskPolicyInput<'_>,
) -> Result<DerivedRiskPolicy, CapabilityPolicyError> {
    verify_assignment_authority(assignment, repo_root)?;
    let normalized_changed_paths = input
        .changed_paths
        .iter()
        .map(|path| normalize_repo_relative_path(repo_root, path))
        .collect::<Result<Vec<_>, _>>()?;
    let matched_high_risk_path = normalized_changed_paths.iter().try_fold(
        false,
        |matched, path| -> Result<bool, CapabilityPolicyError> {
            Ok(matched || scopes_cover_path(repo_root, input.configured_high_risk_paths, path)?)
        },
    )?;
    let configured_contracts = input
        .configured_high_risk_contracts
        .iter()
        .filter_map(|contract| normalize_contract_id(contract))
        .collect::<BTreeSet<_>>();
    let matched_high_risk_contract = input
        .touched_contracts
        .iter()
        .filter_map(|contract| normalize_contract_id(contract))
        .any(|contract| configured_contracts.contains(&contract));
    let facts = RiskFacts {
        configured_high_risk_path: matched_high_risk_path || matched_high_risk_contract,
        cross_owner_scope: input.cross_owner_scope
            || assignment.capability_profile == CapabilityProfile::IntegratorSourceWrite,
        domains: input.named_domains.iter().copied().collect::<BTreeSet<_>>(),
        non_generated_changed_files: input.non_generated_changed_files,
        non_generated_changed_lines: input.non_generated_changed_lines,
        focused_validation_succeeded: input.focused_validation_succeeded,
        ownership_conflict: input.ownership_conflict,
        drift: input.drift,
    };
    let decision = evaluate_closed_risk_gate(&facts);
    Ok(DerivedRiskPolicy {
        facts,
        decision,
        matched_high_risk_path,
        matched_high_risk_contract,
    })
}

fn classify_collaboration_tool(name: &str) -> TypedToolClass {
    if matches_name(name, &["send_message", "wait_agent", "list_agents"]) {
        TypedToolClass::AgentCommunication
    } else if matches_name(
        name,
        &["get_agent_task", "submit_agent_receipt", "set_agent_gate"],
    ) {
        TypedToolClass::OwnTask
    } else if matches_name(
        name,
        &[
            "spawn_agent",
            "send_input",
            "followup_task",
            "interrupt_agent",
            "amend_agent_task",
            "waive_agent_gate",
            "abandon_agent_task",
        ],
    ) {
        TypedToolClass::RootTaskControl
    } else {
        TypedToolClass::Unknown
    }
}

fn empty_authorization() -> AuthorizedToolCall {
    AuthorizedToolCall {
        normalized_repo_paths: Vec::new(),
    }
}

fn authorize_structured_edit(
    assignment: &Assignment,
    repo_root: &Path,
    paths: &[String],
) -> Result<AuthorizedToolCall, CapabilityPolicyError> {
    if !matches!(
        assignment.capability_profile,
        CapabilityProfile::ScopedSourceWrite | CapabilityProfile::IntegratorSourceWrite
    ) {
        return Err(CapabilityPolicyError::ToolDenied {
            profile: assignment.capability_profile,
            class: TypedToolClass::StructuredEdit,
        });
    }
    if paths.is_empty() {
        return Err(CapabilityPolicyError::MissingStructuredEditPaths);
    }
    let mut normalized_repo_paths = Vec::with_capacity(paths.len());
    for path in paths {
        let normalized = normalize_repo_relative_path(repo_root, path)?;
        if !scopes_cover_path(repo_root, &assignment.write_scope, &normalized)? {
            return Err(CapabilityPolicyError::PathOutsideWriteScope(normalized));
        }
        normalized_repo_paths.push(normalized);
    }
    normalized_repo_paths.sort_by(|left, right| compare_repo_paths(left, right));
    normalized_repo_paths.dedup_by(|left, right| repo_paths_equal(left, right));
    Ok(AuthorizedToolCall {
        normalized_repo_paths,
    })
}

fn profile_allows_diff(profile: CapabilityProfile) -> bool {
    matches!(
        profile,
        CapabilityProfile::ReadSearchDiff
            | CapabilityProfile::ScopedSourceWrite
            | CapabilityProfile::IntegratorSourceWrite
    )
}

fn profile_allows_shell(profile: CapabilityProfile) -> bool {
    matches!(
        profile,
        CapabilityProfile::ReadSearchShell
            | CapabilityProfile::ScopedSourceWrite
            | CapabilityProfile::IntegratorSourceWrite
    )
}

fn scopes_cover_path(
    repo_root: &Path,
    scopes: &[RepoScope],
    normalized_path: &str,
) -> Result<bool, CapabilityPolicyError> {
    for scope in scopes {
        let normalized_scope = normalize_repo_relative_path(repo_root, &scope.path)?;
        if scope_covers_path(&normalized_scope, scope.recursive, normalized_path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize_repo_relative_path(
    repo_root: &Path,
    path: &str,
) -> Result<String, CapabilityPolicyError> {
    let normalized = normalize_repo_path_lexically(path)?;
    let canonical_root = canonical_repository_root(repo_root)?;
    ensure_canonical_containment(&canonical_root, &normalized, path)?;
    Ok(normalized)
}

/// Converts one absolute local path into the repository-relative identity used by typed-task
/// scopes. Windows path prefixes and components are compared case-insensitively, while the
/// returned spelling remains stable for evidence and receipt display.
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

    let relative = if cfg!(windows) {
        let root_components = repo_root.components().collect::<Vec<_>>();
        let path_components = path.components().collect::<Vec<_>>();
        if path_components.len() < root_components.len()
            || !path_components
                .iter()
                .zip(&root_components)
                .all(|(candidate, root)| {
                    candidate
                        .as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&root.as_os_str().to_string_lossy())
                })
        {
            return Err(invalid_path(
                &path.to_string_lossy(),
                "absolute path is outside the assignment repository",
            ));
        }
        path_components[root_components.len()..]
            .iter()
            .map(|component| component.as_os_str())
            .collect::<PathBuf>()
    } else {
        path.strip_prefix(repo_root)
            .map(Path::to_path_buf)
            .map_err(|_| {
                invalid_path(
                    &path.to_string_lossy(),
                    "absolute path is outside the assignment repository",
                )
            })?
    };
    normalize_repo_relative_path(repo_root, &relative.to_string_lossy())
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
    Ok(())
}

fn repository_identity(repo_root: &Path) -> Result<String, CapabilityPolicyError> {
    let canonical_root = canonical_repository_root(repo_root)?;
    let canonical_path = canonical_root.to_string_lossy().into_owned();
    let identity_input = if cfg!(windows) {
        canonical_path.to_lowercase()
    } else {
        canonical_path
    };
    Ok(format!("{:x}", Sha256::digest(identity_input.as_bytes())))
}

fn canonical_repository_root(repo_root: &Path) -> Result<PathBuf, CapabilityPolicyError> {
    std::fs::canonicalize(repo_root).map_err(|error| CapabilityPolicyError::InvalidRepositoryRoot {
        path: repo_root.to_string_lossy().into_owned(),
        reason: error.to_string(),
    })
}

fn ensure_canonical_containment(
    canonical_root: &Path,
    normalized_relative: &str,
    original_path: &str,
) -> Result<(), CapabilityPolicyError> {
    let target =
        canonical_root.join(normalized_relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut existing = target.as_path();
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
    if !path_starts_with(&canonical_existing, canonical_root) {
        return Err(invalid_path(
            original_path,
            "path resolves outside the repository through a symlink",
        ));
    }
    Ok(())
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let path_components = path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>();
        let root_components = root
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .collect::<Vec<_>>();
        path_components.starts_with(&root_components)
    } else {
        path.starts_with(root)
    }
}

fn scope_covers_path(scope: &str, recursive: bool, path: &str) -> bool {
    repo_paths_equal(scope, path)
        || recursive
            && normalized_comparison_path(path)
                .strip_prefix(&normalized_comparison_path(scope))
                .is_some_and(|suffix| suffix.starts_with('/'))
}

fn compare_repo_paths(left: &str, right: &str) -> Ordering {
    normalized_comparison_path(left)
        .cmp(&normalized_comparison_path(right))
        .then_with(|| left.cmp(right))
}

fn repo_paths_equal(left: &str, right: &str) -> bool {
    normalized_comparison_path(left) == normalized_comparison_path(right)
}

fn normalized_comparison_path(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

fn evaluate_closed_risk_gate(facts: &RiskFacts) -> RiskGateDecision {
    let mut reasons = Vec::new();
    if facts.configured_high_risk_path {
        reasons.push("configured high-risk contract or path".to_string());
    }
    if facts.cross_owner_scope {
        reasons.push("cross-owner scope".to_string());
    }
    for domain in &facts.domains {
        reasons.push(risk_domain_reason(*domain).to_string());
    }
    if facts.non_generated_changed_files > 5 {
        reasons.push("more than five non-generated changed files".to_string());
    }
    if facts.non_generated_changed_lines > 400 {
        reasons.push("more than 400 non-generated changed lines".to_string());
    }
    if !facts.focused_validation_succeeded {
        reasons.push("missing successful focused validation".to_string());
    }
    if facts.ownership_conflict {
        reasons.push("ownership conflict".to_string());
    }
    if facts.drift {
        reasons.push("concurrent drift".to_string());
    }
    RiskGateDecision {
        review_required: !reasons.is_empty(),
        reasons,
    }
}

fn normalize_contract_id(contract: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut pending_separator = false;
    for character in contract.trim().chars() {
        if character.is_whitespace() || matches!(character, '-' | '_') {
            pending_separator = !normalized.is_empty();
            continue;
        }
        if pending_separator {
            normalized.push('-');
            pending_separator = false;
        }
        normalized.extend(character.to_lowercase());
    }
    (!normalized.is_empty()).then_some(normalized)
}

fn risk_domain_reason(domain: RiskDomain) -> &'static str {
    match domain {
        RiskDomain::Concurrency => "concurrency risk",
        RiskDomain::UnsafeCode => "unsafe risk",
        RiskDomain::Lifecycle => "lifecycle risk",
        RiskDomain::Persistence => "persistence risk",
        RiskDomain::Schema => "schema risk",
        RiskDomain::Protocol => "protocol risk",
        RiskDomain::Security => "security risk",
        RiskDomain::Installation => "installation risk",
    }
}

fn invalid_path(path: &str, reason: impl Into<String>) -> CapabilityPolicyError {
    CapabilityPolicyError::InvalidRepoPath {
        path: path.to_string(),
        reason: reason.into(),
    }
}

fn matches_name(name: &str, candidates: &[&str]) -> bool {
    candidates.contains(&name)
}

fn starts_with_external_prefix(name: &str) -> bool {
    ["mcp__", "extension__", "dynamic__"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
#[path = "task_capabilities_tests.rs"]
mod tests;
