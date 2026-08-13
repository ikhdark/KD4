use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::Path;

use crate::AssignmentId;
use crate::AttemptId;
use crate::MutationEventId;
use crate::RepoScope;
use crate::StoreError;
use crate::StoreResult;
use crate::WakeEventId;
use crate::normalize_repo_scopes;
use crate::scope::repository_identity;

pub const DEFAULT_OBSERVATION_LIMIT: usize = 20;
pub const MAX_OBSERVATION_LIMIT: usize = 100;
pub const MAX_WAKE_EVENTS_PER_ROOT: usize = 256;
pub const MAX_WAKE_EVENTS_PER_READ: usize = 50;
pub const DEFAULT_BINDING_LIMIT: usize = 100;
pub const MAX_BINDING_LIMIT: usize = 256;
pub const DEFAULT_MUTATION_EVIDENCE_LIMIT: usize = 20;
pub const MAX_MUTATION_EVIDENCE_LIMIT: usize = 100;
pub const MAX_VALIDATION_CALLS_PER_TASK: usize = 100;
pub const DEFAULT_SNAPSHOT_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_SNAPSHOT_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_MUTATION_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_WORKSPACE_LEASE_SECONDS: i64 = 120;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Architect,
    Explorer,
    Worker,
    Reviewer,
    Verifier,
    Integrator,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityProfile {
    ReadSearch,
    ReadSearchDiff,
    ReadSearchShell,
    ScopedSourceWrite,
    IntegratorSourceWrite,
}

impl AgentRole {
    pub fn capability_profile(self) -> CapabilityProfile {
        match self {
            Self::Architect | Self::Explorer => CapabilityProfile::ReadSearch,
            Self::Worker => CapabilityProfile::ScopedSourceWrite,
            Self::Reviewer => CapabilityProfile::ReadSearchDiff,
            Self::Verifier => CapabilityProfile::ReadSearchShell,
            Self::Integrator => CapabilityProfile::IntegratorSourceWrite,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Review,
    Verification,
    Integration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AssignmentRelation {
    pub kind: RelationKind,
    pub target_assignment_ids: Vec<AssignmentId>,
}

pub const ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION: u32 = 1;

/// The worker contract authored by a distinct read-only Architect. The sealed
/// receipt is the single source of truth; assignment fields are projections
/// checked against its canonical normalized representation at admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchitectureContractV1 {
    pub schema_version: u32,
    pub objective: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub read_scope: Vec<RepoScope>,
    #[serde(default)]
    pub write_scope: Vec<RepoScope>,
    pub stop_condition: String,
    #[serde(default)]
    pub risk_hints: Vec<String>,
    #[serde(default)]
    pub required_evidence: Vec<String>,
    #[serde(default)]
    pub prohibited_changes: Vec<String>,
    #[serde(default)]
    pub contract_claims: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SealedArchitectureContractV1 {
    pub contract: ArchitectureContractV1,
    pub contract_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchitectureContractRef {
    pub architect_assignment_id: AssignmentId,
    pub architect_attempt_id: AttemptId,
    pub contract_version: u32,
    pub contract_sha256: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStrategy {
    #[default]
    Auto,
    Shared,
    Isolated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AssignmentDraft {
    pub root_session_id: String,
    pub role: AgentRole,
    pub capability_profile: CapabilityProfile,
    pub objective: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub read_scope: Vec<RepoScope>,
    #[serde(default)]
    pub write_scope: Vec<RepoScope>,
    pub stop_condition: String,
    #[serde(default)]
    pub dependencies: Vec<AssignmentId>,
    #[serde(default)]
    pub risk_hints: Vec<String>,
    #[serde(default)]
    pub required_evidence: Vec<String>,
    #[serde(default)]
    pub prohibited_changes: Vec<String>,
    #[serde(default)]
    pub contract_claims: Vec<String>,
    #[serde(default)]
    pub workspace_strategy: WorkspaceStrategy,
    pub relation: Option<AssignmentRelation>,
    #[serde(default)]
    pub architecture_contract_ref: Option<ArchitectureContractRef>,
}

/// Stable, bounded identity for an Explorer's declared primary question and surface.
/// Supporting-read history is deliberately excluded.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimaryInvestigationIdentity(String);

impl PrimaryInvestigationIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdmissionOverlapSummary {
    pub benign_read_overlap_count: u32,
    pub duplicated_primary_investigation_count: u32,
    pub conflicting_write_read_count: u32,
    pub conflicting_write_write_count: u32,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionRejectionReason {
    DuplicateExplorerInvestigation,
    CorrectnessSensitiveReadConflict,
    ActiveValidationConflict,
    IsolatedIntegratorUnavailable,
}

impl std::fmt::Display for AdmissionRejectionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::DuplicateExplorerInvestigation => "duplicate_explorer_investigation",
            Self::CorrectnessSensitiveReadConflict => "correctness_sensitive_read_conflict",
            Self::ActiveValidationConflict => "active_validation_conflict",
            Self::IsolatedIntegratorUnavailable => "isolated_integrator_unavailable",
        };
        formatter.write_str(value)
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrationPlan {
    SingleWriter,
    RootOwned,
    TypedIntegratorRequired,
}

#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedAssignment {
    pub assignment: Assignment,
    pub attempt: Attempt,
    pub overlaps: AdmissionOverlapSummary,
    pub integration_plan: IntegrationPlan,
}

impl AssignmentDraft {
    pub fn normalize(self, repo_root: &Path) -> StoreResult<Assignment> {
        validate_nonempty("root_session_id", &self.root_session_id)?;
        validate_nonempty("objective", &self.objective)?;
        validate_nonempty("stop_condition", &self.stop_condition)?;
        if self.capability_profile != self.role.capability_profile() {
            return Err(StoreError::InvalidAssignment(format!(
                "role {:?} requires capability profile {:?}",
                self.role,
                self.role.capability_profile()
            )));
        }
        if self.acceptance_criteria.is_empty() {
            return Err(StoreError::InvalidAssignment(
                "at least one acceptance criterion is required".to_string(),
            ));
        }
        let mut criterion_ids = HashSet::new();
        for criterion in &self.acceptance_criteria {
            validate_nonempty("criterion id", &criterion.id)?;
            validate_nonempty("criterion text", &criterion.text)?;
            if !criterion_ids.insert(criterion.id.as_str()) {
                return Err(StoreError::InvalidAssignment(format!(
                    "duplicate acceptance criterion id {}",
                    criterion.id
                )));
            }
        }
        let mut dependency_ids = HashSet::new();
        for dependency in &self.dependencies {
            if !dependency_ids.insert(*dependency) {
                return Err(StoreError::InvalidAssignment(format!(
                    "duplicate dependency {dependency}"
                )));
            }
        }
        if let Some(contract_ref) = self.architecture_contract_ref.as_ref() {
            if self.role != AgentRole::Worker {
                return Err(StoreError::InvalidAssignment(
                    "only workers may reference a sealed architecture contract".to_string(),
                ));
            }
            if !dependency_ids.contains(&contract_ref.architect_assignment_id) {
                return Err(StoreError::InvalidAssignment(
                    "architecture contract assignment must be an explicit dependency".to_string(),
                ));
            }
            if contract_ref.contract_version != ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION
                || contract_ref.contract_sha256.trim().is_empty()
            {
                return Err(StoreError::InvalidAssignment(
                    "architecture contract reference has an unsupported version or empty hash"
                        .to_string(),
                ));
            }
        }
        let mut required_evidence = HashSet::new();
        for requirement in &self.required_evidence {
            validate_nonempty("required evidence", requirement)?;
            if !required_evidence.insert(requirement.as_str()) {
                return Err(StoreError::InvalidAssignment(format!(
                    "duplicate required evidence {requirement}"
                )));
            }
        }
        let mut contract_claims = HashSet::new();
        for contract in &self.contract_claims {
            validate_nonempty("contract claim", contract)?;
            if !contract_claims.insert(contract.as_str()) {
                return Err(StoreError::InvalidAssignment(format!(
                    "duplicate contract claim {contract}"
                )));
            }
        }

        let read_scope = normalize_repo_scopes(repo_root, &self.read_scope)?;
        let write_scope = normalize_repo_scopes(repo_root, &self.write_scope)?;
        validate_role_relation(
            self.role,
            &write_scope,
            self.relation.as_ref(),
            &dependency_ids,
        )?;

        Ok(Assignment {
            assignment_id: AssignmentId::new(),
            root_session_id: self.root_session_id,
            repository_id: repository_identity(repo_root)?.id,
            role: self.role,
            capability_profile: self.capability_profile,
            objective: self.objective,
            acceptance_criteria: self.acceptance_criteria,
            read_scope,
            write_scope,
            stop_condition: self.stop_condition,
            dependencies: self.dependencies,
            risk_hints: self.risk_hints,
            required_evidence: self.required_evidence,
            prohibited_changes: self.prohibited_changes,
            contract_claims: self.contract_claims,
            workspace_strategy: self.workspace_strategy,
            workspace_id: String::new(),
            start_epoch: 0,
            relation: self.relation,
            architecture_contract_ref: self.architecture_contract_ref,
            task_capsule: None,
            created_at: Utc::now(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Assignment {
    pub assignment_id: AssignmentId,
    pub root_session_id: String,
    /// Stable hash of the Git repository lineage (or canonical root outside Git). The private
    /// absolute worktree root is stored separately and never included in task-facing JSON.
    #[serde(default)]
    pub repository_id: String,
    /// Stable identity of the concrete shared or isolated worktree.
    #[serde(default)]
    pub workspace_id: String,
    pub role: AgentRole,
    pub capability_profile: CapabilityProfile,
    pub objective: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    pub read_scope: Vec<RepoScope>,
    pub write_scope: Vec<RepoScope>,
    pub stop_condition: String,
    pub dependencies: Vec<AssignmentId>,
    pub risk_hints: Vec<String>,
    pub required_evidence: Vec<String>,
    pub prohibited_changes: Vec<String>,
    #[serde(default)]
    pub contract_claims: Vec<String>,
    #[serde(default)]
    pub workspace_strategy: WorkspaceStrategy,
    #[serde(default)]
    pub start_epoch: u64,
    pub relation: Option<AssignmentRelation>,
    #[serde(default)]
    pub architecture_contract_ref: Option<ArchitectureContractRef>,
    /// Immutable canonical `TaskCapsuleV1` JSON attached before the child is launched.
    #[serde(default)]
    pub task_capsule: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Assignment {
    pub fn primary_investigation_identity(&self) -> Option<PrimaryInvestigationIdentity> {
        if self.role != AgentRole::Explorer {
            return None;
        }
        let mut scopes = self.read_scope.clone();
        scopes.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.recursive.cmp(&right.recursive))
        });
        let mut claims = self.contract_claims.clone();
        claims.sort();
        let mut criteria = self.acceptance_criteria.clone();
        criteria.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.text.cmp(&right.text))
        });
        let canonical =
            serde_json::to_vec(&(scopes, claims, self.objective.trim(), criteria)).ok()?;
        Some(PrimaryInvestigationIdentity(format!(
            "{:x}",
            Sha256::digest(&canonical)
        )))
    }

    pub(crate) fn validate_selective_role_contract(&self) -> StoreResult<()> {
        let invalid = |detail: &str| Err(StoreError::InvalidAssignment(detail.to_string()));
        match self.role {
            AgentRole::Architect => {
                if self.read_scope.is_empty() {
                    return invalid("architects require a non-empty architecture scope");
                }
                if !self.required_evidence.is_empty() {
                    return invalid("architects cannot require focused validation proofs");
                }
            }
            AgentRole::Explorer => {
                if self.read_scope.is_empty() {
                    return invalid("explorers require a non-empty primary investigation scope");
                }
                if !self.required_evidence.is_empty() {
                    return invalid("explorers cannot require focused validation proofs");
                }
            }
            AgentRole::Worker => {
                if self.write_scope.is_empty() {
                    return invalid("workers require a non-empty owned write scope");
                }
                if self.required_evidence.is_empty() {
                    return invalid("workers require at least one proof obligation");
                }
            }
            AgentRole::Reviewer => {
                if !self.required_evidence.is_empty() {
                    return invalid(
                        "reviewers report diff/findings/gate evidence and cannot own focused proof obligations",
                    );
                }
            }
            AgentRole::Verifier => {
                if self.required_evidence.is_empty() {
                    return invalid("verifiers require at least one focused proof obligation");
                }
            }
            AgentRole::Integrator => {
                if self.required_evidence.is_empty() {
                    return invalid(
                        "integrators require at least one integration proof obligation",
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelevantHandle {
    File { path: String },
    Symbol { path: String, symbol: String },
}

impl RelevantHandle {
    pub fn path(&self) -> &str {
        match self {
            Self::File { path } | Self::Symbol { path, .. } => path,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskCapsuleHandle {
    File {
        path: String,
        existed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
    Symbol {
        path: String,
        symbol: String,
        existed: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskCapsuleV1 {
    pub schema_version: u8,
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub role: AgentRole,
    pub capability_profile: CapabilityProfile,
    pub requirements: Vec<AcceptanceCriterion>,
    pub objective: String,
    pub read_scope: Vec<RepoScope>,
    pub write_scope: Vec<RepoScope>,
    pub relevant_handles: Vec<TaskCapsuleHandle>,
    pub workspace_epoch: u64,
    pub workspace_manifest_hash: String,
    pub prohibited_changes: Vec<String>,
    pub required_evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttemptAmendment {
    pub reason: String,
    pub objective: Option<String>,
    pub acceptance_criteria: Option<Vec<AcceptanceCriterion>>,
    pub stop_condition: Option<String>,
}

impl AttemptAmendment {
    pub(crate) fn validate(&self) -> StoreResult<()> {
        validate_nonempty("amendment reason", &self.reason)?;
        if let Some(objective) = &self.objective {
            validate_nonempty("amended objective", objective)?;
        }
        if let Some(stop_condition) = &self.stop_condition {
            validate_nonempty("amended stop condition", stop_condition)?;
        }
        if let Some(criteria) = &self.acceptance_criteria {
            if criteria.is_empty() {
                return Err(StoreError::InvalidAssignment(
                    "amended acceptance criteria cannot be empty".to_string(),
                ));
            }
            let mut ids = HashSet::new();
            for criterion in criteria {
                validate_nonempty("amended criterion id", &criterion.id)?;
                validate_nonempty("amended criterion text", &criterion.text)?;
                if !ids.insert(criterion.id.as_str()) {
                    return Err(StoreError::InvalidAssignment(format!(
                        "duplicate amended criterion id {}",
                        criterion.id
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Active,
    Completed,
    NeedsMain,
    Violated,
    Abandoned,
}

impl AttemptState {
    pub fn is_terminal(self) -> bool {
        self != Self::Active
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attempt {
    pub attempt_id: AttemptId,
    pub assignment_id: AssignmentId,
    pub ordinal: u8,
    pub amendment: Option<AttemptAmendment>,
    pub state: AttemptState,
    pub created_at: DateTime<Utc>,
    pub sealed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskActor {
    Root,
    Attempt(AttemptId),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceActorKind {
    Root,
    Typed,
    Legacy,
    External,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Active,
    Expired,
    Released,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct WorkspaceManifestEntry {
    pub path: String,
    pub content_hash: Option<String>,
    pub existed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceCasMismatchDetail {
    pub path: String,
    pub expected: Option<WorkspaceManifestEntry>,
    pub current: Option<WorkspaceManifestEntry>,
    pub current_epoch: Option<u64>,
    pub last_writer: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceRevision {
    pub repository_id: String,
    pub workspace_id: String,
    pub epoch: u64,
    pub manifest_hash: String,
    pub files: Vec<WorkspaceManifestEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceActorRegistration {
    pub root_session_id: String,
    pub actor_id: String,
    pub kind: WorkspaceActorKind,
    pub assignment_id: Option<AssignmentId>,
    pub attempt_id: Option<AttemptId>,
    #[serde(default)]
    pub strategy: WorkspaceStrategy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceMutationRequest {
    pub root_session_id: String,
    pub actor_id: String,
    pub kind: WorkspaceActorKind,
    pub attempt_id: Option<AttemptId>,
    pub paths: Vec<String>,
    #[serde(default)]
    pub contracts: Vec<String>,
    #[serde(default)]
    pub expected_manifest: Vec<WorkspaceManifestEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceMutationLease {
    pub lease_id: String,
    pub repository_id: String,
    pub workspace_id: String,
    pub root_session_id: String,
    pub actor_id: String,
    pub kind: WorkspaceActorKind,
    pub attempt_id: Option<AttemptId>,
    pub start_epoch: u64,
    pub paths: Vec<String>,
    pub contracts: Vec<String>,
    pub expected_manifest: Vec<WorkspaceManifestEntry>,
    pub state: LeaseState,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceMutationResult {
    pub lease_id: String,
    pub start_epoch: u64,
    pub end_epoch: u64,
    pub changed_paths: Vec<String>,
    pub drift_paths: Vec<String>,
}

/// Internal optimization receipt for a strongly constructed workspace manifest.
/// This is deliberately not serialized into command or protocol schemas.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceManifestReceipt {
    pub(crate) repository_id: String,
    pub(crate) workspace_id: String,
    pub(crate) epoch: u64,
    pub(crate) paths: Vec<String>,
    pub(crate) manifest_id: String,
    pub(crate) entries: Vec<WorkspaceManifestEntry>,
}

impl WorkspaceManifestReceipt {
    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub fn entries(&self) -> &[WorkspaceManifestEntry] {
        &self.entries
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceManifestWork {
    pub overlay_traversals: u64,
    pub files_examined: u64,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub git_subprocesses: u64,
    pub manifests_constructed: u64,
    pub full_constructions: u64,
    pub incremental_constructions: u64,
    pub reuse_hits: u64,
    pub reuse_rejections: u64,
    pub unique_payloads: u64,
    pub payload_reuses: u64,
    pub manifest_bytes_persisted: u64,
    pub reference_bytes_persisted: u64,
    pub sqlite_statements: u64,
    pub transaction_micros: u64,
    pub duration_micros: u64,
}

#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedWorkspaceManifest {
    pub(crate) receipt: WorkspaceManifestReceipt,
    pub(crate) canonical: crate::manifest_storage::CanonicalManifest,
    pub(crate) dependency_identity: Option<String>,
    pub(crate) work: WorkspaceManifestWork,
}

impl PreparedWorkspaceManifest {
    pub(crate) fn new(
        receipt: WorkspaceManifestReceipt,
        work: WorkspaceManifestWork,
    ) -> StoreResult<Self> {
        let canonical = crate::manifest_storage::canonical_manifest(&receipt.entries)?;
        if canonical.manifest_hash != receipt.manifest_id {
            return Err(StoreError::CorruptData(
                "prepared workspace receipt hash does not match its canonical payload".to_string(),
            ));
        }
        Ok(Self {
            receipt,
            canonical,
            dependency_identity: None,
            work,
        })
    }

    pub fn receipt(&self) -> &WorkspaceManifestReceipt {
        &self.receipt
    }

    pub fn work(&self) -> WorkspaceManifestWork {
        self.work
    }

    pub fn canonical_byte_count(&self) -> usize {
        self.canonical.bytes.len()
    }

    pub fn dependency_identity(&self) -> Option<&str> {
        self.dependency_identity.as_deref()
    }

    pub fn with_dependency_identity(mut self, dependency_identity: String) -> Self {
        self.dependency_identity = Some(dependency_identity);
        self
    }

    pub fn reused_after_validation(
        mut self,
        overlay_traversals: u64,
        files_examined: u64,
        git_subprocesses: u64,
        duration_micros: u64,
    ) -> Self {
        self.work = WorkspaceManifestWork {
            overlay_traversals,
            files_examined,
            git_subprocesses,
            reuse_hits: 1,
            duration_micros,
            ..WorkspaceManifestWork::default()
        };
        self
    }

    pub fn with_revalidation_work(
        mut self,
        overlay_traversals: u64,
        files_examined: u64,
        git_subprocesses: u64,
        reuse_rejections: u64,
        duration_micros: u64,
    ) -> Self {
        self.work.overlay_traversals = self
            .work
            .overlay_traversals
            .saturating_add(overlay_traversals);
        self.work.files_examined = self.work.files_examined.saturating_add(files_examined);
        self.work.git_subprocesses = self.work.git_subprocesses.saturating_add(git_subprocesses);
        self.work.reuse_rejections = self.work.reuse_rejections.saturating_add(reuse_rejections);
        self.work.duration_micros = self.work.duration_micros.saturating_add(duration_micros);
        self
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMutationFinishOutcome {
    pub(crate) result: WorkspaceMutationResult,
    pub(crate) final_manifest: Option<PreparedWorkspaceManifest>,
    pub(crate) work: WorkspaceManifestWork,
}

impl WorkspaceMutationFinishOutcome {
    pub fn result(&self) -> &WorkspaceMutationResult {
        &self.result
    }

    pub fn final_manifest(&self) -> Option<&PreparedWorkspaceManifest> {
        self.final_manifest.as_ref()
    }

    pub fn work(&self) -> WorkspaceManifestWork {
        self.work
    }

    pub fn into_result(self) -> WorkspaceMutationResult {
        self.result
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceFinalizationFence {
    pub fence_id: String,
    pub repository_id: String,
    pub workspace_id: String,
    pub root_session_id: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEvent {
    pub workspace_id: String,
    pub epoch: u64,
    pub actor_id: Option<String>,
    pub actor_kind: WorkspaceActorKind,
    pub attribution_confidence: AttributionConfidence,
    pub paths: Vec<String>,
    pub contracts: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceTaskStatus {
    pub epoch: u64,
    pub last_progress_at: Option<DateTime<Utc>>,
    pub lease_state: Option<LeaseState>,
    pub pending_gates: Vec<GateKind>,
    pub stale_reason: Option<String>,
    pub next_required_action: Option<String>,
    pub nudge_sent_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationHandoffState {
    Ready,
    Claimed,
    Integrated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IsolationHandoff {
    pub assignment_id: AssignmentId,
    pub source_workspace_id: String,
    #[serde(default)]
    pub source_repository_root: Option<String>,
    pub source_epoch: u64,
    pub source_manifest_hash: String,
    pub covered_manifest: Vec<WorkspaceManifestEntry>,
    pub state: IsolationHandoffState,
    pub integrator_assignment_id: Option<AssignmentId>,
    pub created_at: DateTime<Utc>,
    pub integrated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuiescenceStatus {
    pub quiescent: bool,
    pub active_assignment_ids: Vec<AssignmentId>,
    pub running_validation_call_ids: Vec<String>,
    pub pending_gate_assignment_ids: Vec<AssignmentId>,
    pub active_claim_assignment_ids: Vec<AssignmentId>,
    #[serde(default)]
    pub active_mutation_lease_ids: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl TaskActor {
    pub(crate) fn require_root(self) -> StoreResult<()> {
        match self {
            Self::Root => Ok(()),
            Self::Attempt(_) => Err(StoreError::RootAuthorityRequired),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    Accepted,
    Starting,
    Reading,
    Editing,
    Reviewing,
    Validating,
    Blocked,
    ToolCall,
    Mutation,
    GateChanged,
    ReceiptSealed,
    Completed,
    NeedsMain,
    Violated,
    Abandoned,
}

impl ObservationKind {
    #[doc(hidden)]
    pub fn is_meaningful_progress(self) -> bool {
        matches!(
            self,
            Self::Reading
                | Self::Reviewing
                | Self::Validating
                | Self::Blocked
                | Self::Mutation
                | Self::GateChanged
                | Self::ReceiptSealed
                | Self::Completed
                | Self::NeedsMain
                | Self::Violated
                | Self::Abandoned
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeObservation {
    pub event_id: MutationEventId,
    pub wake_event_id: WakeEventId,
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub kind: ObservationKind,
    pub summary: String,
    pub call_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCallStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}

impl ValidationCallStatus {
    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }

    pub fn is_success(self) -> bool {
        self == Self::Succeeded
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationProofKind {
    #[default]
    LegacyUnclassified,
    Focused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationCall {
    pub call_id: String,
    pub attempt_id: AttemptId,
    pub command_summary: String,
    #[serde(default)]
    pub resolved_executable: Option<String>,
    #[serde(default)]
    pub proof_kind: ValidationProofKind,
    #[serde(default)]
    pub evidence: ValidationEvidence,
    pub status: ValidationCallStatus,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationEvidence {
    /// Immutable completion-candidate identity. Missing values mark legacy
    /// evidence that may be displayed but must never satisfy proof reuse.
    #[serde(default)]
    pub candidate_id: String,
    #[serde(default)]
    pub implementation_identity: String,
    #[serde(default)]
    pub source_evidence_epoch: Option<u64>,
    #[serde(default)]
    pub normalized_invocation: String,
    #[serde(default)]
    pub coverage_identity: String,
    pub start_epoch: u64,
    pub end_epoch: Option<u64>,
    #[serde(default)]
    pub covered_scopes: Vec<RepoScope>,
    pub covered_manifest: Vec<WorkspaceManifestEntry>,
    #[serde(default)]
    pub execution_snapshot: Option<Box<ValidationExecutionSnapshot>>,
    pub covered_contracts: Vec<String>,
    pub manifest_hash: String,
    pub repository_wide: bool,
    pub cwd: Option<String>,
    pub environment_hash: Option<String>,
    pub toolchain: Option<String>,
    #[serde(default)]
    pub features_configuration_identity: String,
    #[serde(default)]
    pub covered_input_manifest_hash: String,
    #[serde(default)]
    pub dependency_manifest_hash: String,
    #[serde(default)]
    pub successful_result: Option<bool>,
    #[serde(default)]
    pub retained_output_digest: String,
    pub retained_output_ref: Option<String>,
    #[serde(default)]
    pub output_summary: Option<String>,
    /// Backward-compatible structured terminal projection. The task store
    /// remains protocol-agnostic and preserves the canonical JSON value.
    #[serde(default)]
    pub validation_result: Option<serde_json::Value>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub shared_from_call_id: Option<String>,
    pub stale_reason: Option<String>,
}

impl ValidationEvidence {
    pub fn has_complete_request_identity(&self) -> bool {
        !self.candidate_id.is_empty()
            && !self.implementation_identity.is_empty()
            && self.source_evidence_epoch.is_some()
            && !self.normalized_invocation.is_empty()
            && !self.coverage_identity.is_empty()
            && !self.manifest_hash.is_empty()
            && self.cwd.as_deref().is_some_and(|cwd| !cwd.is_empty())
            && self
                .environment_hash
                .as_deref()
                .is_some_and(|hash| !hash.is_empty())
            && self
                .toolchain
                .as_deref()
                .is_some_and(|toolchain| !toolchain.is_empty())
            && !self.covered_input_manifest_hash.is_empty()
            && !self.dependency_manifest_hash.is_empty()
            && !self.features_configuration_identity.is_empty()
    }

    pub fn is_reusable_success(&self) -> bool {
        self.has_complete_request_identity()
            && self.successful_result == Some(true)
            && !self.retained_output_digest.is_empty()
            && self
                .retained_output_ref
                .as_deref()
                .is_some_and(|output_ref| !output_ref.is_empty())
            && self.stale_reason.is_none()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationExecutionSnapshot {
    pub manifest: Vec<WorkspaceManifestEntry>,
    pub manifest_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatusClaim {
    Completed,
    NeedsMain,
    Blocked,
    Failed,
    Violated,
    Abandoned,
}

impl AgentStatusClaim {
    pub fn is_success(self) -> bool {
        self == Self::Completed
    }

    pub(crate) fn attempt_state(self) -> AttemptState {
        match self {
            Self::Completed => AttemptState::Completed,
            Self::Violated => AttemptState::Violated,
            Self::Abandoned => AttemptState::Abandoned,
            Self::NeedsMain | Self::Blocked | Self::Failed => AttemptState::NeedsMain,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CriterionResult {
    pub criterion_id: String,
    pub status: CriterionStatus,
    pub evidence: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeclaredChange {
    pub path: String,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReceiptDraft {
    pub status: AgentStatusClaim,
    pub summary: String,
    pub criterion_results: Vec<CriterionResult>,
    pub declared_changes: Vec<DeclaredChange>,
    pub validation_call_ids: Vec<String>,
    pub blockers: Vec<String>,
    pub risks: Vec<String>,
    pub next_action: Option<String>,
    #[serde(default)]
    pub architecture_contract: Option<ArchitectureContractV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MissingEvidenceObligation {
    /// Stable within the immutable assignment and persisted requirement ordinal.
    pub id: String,
    pub requirement: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentReceipt {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub status: AgentStatusClaim,
    pub summary: String,
    pub criterion_results: Vec<CriterionResult>,
    pub declared_changes: Vec<DeclaredChange>,
    pub validation_call_ids: Vec<String>,
    pub blockers: Vec<String>,
    pub risks: Vec<String>,
    pub next_action: Option<String>,
    #[serde(default)]
    pub architecture_contract: Option<SealedArchitectureContractV1>,
    #[serde(default)]
    pub evidence_epoch: u64,
    #[serde(default)]
    pub evidence_manifest_hash: String,
    pub sealed_at: DateTime<Utc>,
}

/// Bounded result of evaluating the generic productivity deadline against owned operations.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProductivitySummary {
    pub active_owned_operation_count: u32,
    pub cancelled_expired_operation_count: u32,
}

/// Atomic outcome of one nonproductive-assignment recovery evaluation.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NonproductiveRecovery {
    NotEligible,
    Suspended(ProductivitySummary),
    Recovered {
        receipt: Box<AgentReceipt>,
        productivity: ProductivitySummary,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    Risk,
    Review,
    Verification,
    Mutation,
    Ownership,
}

impl GateKind {
    pub fn is_waivable(self) -> bool {
        matches!(self, Self::Review | Self::Verification)
    }
}

impl std::fmt::Display for GateKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}",
            serde_json::to_value(self).unwrap_or_default()
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pending,
    Passed,
    ChangesRequested,
    Failed,
    Waived,
    Violated,
}

impl GateStatus {
    pub fn is_sealed(self) -> bool {
        self != Self::Pending
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentGate {
    pub assignment_id: AssignmentId,
    pub kind: GateKind,
    pub status: GateStatus,
    pub reason: String,
    pub waiver_reason: Option<String>,
    #[serde(default)]
    pub evidence_epoch: u64,
    pub updated_at: DateTime<Utc>,
    pub sealed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriteClaim {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub scopes: Vec<RepoScope>,
    #[serde(default)]
    pub contract_claims: Vec<String>,
    #[serde(default)]
    pub workspace_id: String,
    pub supersedes: Vec<AssignmentId>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriteClaimConflict {
    pub assignment_id: AssignmentId,
    pub existing_scope: RepoScope,
    pub requested_scope: RepoScope,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionConfidence {
    Definitive,
    DetectionOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MutationEvidence {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub path: String,
    pub pre_write_hash: Option<String>,
    pub pre_write_existed: bool,
    pub final_hash: Option<String>,
    #[serde(default)]
    pub final_write_existed: Option<bool>,
    pub mutation_event_ids: Vec<MutationEventId>,
    pub attribution_confidence: AttributionConfidence,
    pub snapshot_retained: bool,
    pub first_observed_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub start_epoch: u64,
    #[serde(default)]
    pub end_epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationSnapshotVersion {
    PreWrite,
    Final,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MutationSnapshotChunk {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub path: String,
    pub version: MutationSnapshotVersion,
    pub existed: bool,
    pub offset: u64,
    pub total_bytes: u64,
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTaskBindingDraft {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub agent_path: String,
    pub task_name: String,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTaskBinding {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub root_session_id: String,
    pub agent_path: String,
    pub task_name: String,
    pub thread_id: Option<String>,
    pub bound_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyState {
    Unknown,
    SelfReference,
    Cyclic,
    Incomplete,
    Blocked,
    Failed,
    Violated,
    Abandoned,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DependencyBlocker {
    pub assignment_id: AssignmentId,
    pub state: DependencyState,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskDomain {
    Concurrency,
    UnsafeCode,
    Lifecycle,
    Persistence,
    Schema,
    Protocol,
    Security,
    Installation,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RiskFacts {
    pub configured_high_risk_path: bool,
    pub cross_owner_scope: bool,
    pub domains: BTreeSet<RiskDomain>,
    pub non_generated_changed_files: u32,
    pub non_generated_changed_lines: u32,
    pub focused_validation_succeeded: bool,
    pub ownership_conflict: bool,
    pub drift: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RiskGateDecision {
    pub review_required: bool,
    pub reasons: Vec<String>,
}

pub const CONCURRENT_DRIFT_REASON: &str = "concurrent drift";

pub fn evaluate_risk_gate(facts: &RiskFacts) -> RiskGateDecision {
    let mut reasons = Vec::new();
    if facts.configured_high_risk_path {
        reasons.push("configured high-risk contract or path".to_string());
    }
    if facts.cross_owner_scope {
        reasons.push("cross-owner scope".to_string());
    }
    for domain in &facts.domains {
        let reason = match domain {
            RiskDomain::Concurrency => "concurrency risk",
            RiskDomain::UnsafeCode => "unsafe risk",
            RiskDomain::Lifecycle => "lifecycle risk",
            RiskDomain::Persistence => "persistence risk",
            RiskDomain::Schema => "schema risk",
            RiskDomain::Protocol => "protocol risk",
            RiskDomain::Security => "security risk",
            RiskDomain::Installation => "installation risk",
        };
        reasons.push(reason.to_string());
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
        reasons.push(CONCURRENT_DRIFT_REASON.to_string());
    }
    RiskGateDecision {
        review_required: !reasons.is_empty(),
        reasons,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTask {
    pub assignment: Assignment,
    pub current_attempt: Attempt,
    pub gates: Vec<AgentGate>,
    pub receipt: Option<AgentReceipt>,
    #[serde(default)]
    pub validation_calls: Vec<ValidationCall>,
    #[serde(default)]
    pub workspace_status: WorkspaceTaskStatus,
    #[serde(default)]
    pub isolation_handoff: Option<IsolationHandoff>,
    #[serde(default)]
    pub integration_handoffs: Vec<IsolationHandoff>,
    pub observations: Vec<RuntimeObservation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WakeEvent {
    pub event_id: WakeEventId,
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub reason: ObservationKind,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WakeRead {
    pub reason: Option<ObservationKind>,
    pub updated_agents: Vec<WakeEvent>,
    pub latest_event_id: Option<WakeEventId>,
    pub truncated_count: u64,
    pub timed_out: bool,
}

fn validate_nonempty(field: &str, value: &str) -> StoreResult<()> {
    if value.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(format!(
            "{field} cannot be empty"
        )));
    }
    Ok(())
}

fn validate_role_relation(
    role: AgentRole,
    write_scope: &[RepoScope],
    relation: Option<&AssignmentRelation>,
    dependencies: &HashSet<AssignmentId>,
) -> StoreResult<()> {
    let relation_targets_are_dependencies = |relation: &AssignmentRelation| {
        relation
            .target_assignment_ids
            .iter()
            .all(|target| dependencies.contains(target))
    };
    match role {
        AgentRole::Architect => {
            if !write_scope.is_empty() || relation.is_some() {
                return Err(StoreError::InvalidAssignment(
                    "architects must be read-only and cannot declare a relation".to_string(),
                ));
            }
        }
        AgentRole::Explorer => {
            if !write_scope.is_empty() || relation.is_some() {
                return Err(StoreError::InvalidAssignment(
                    "explorers must be read-only and cannot declare a relation".to_string(),
                ));
            }
        }
        AgentRole::Worker => {
            if relation.is_some() {
                return Err(StoreError::InvalidAssignment(
                    "workers cannot declare review, verification, or integration relations"
                        .to_string(),
                ));
            }
        }
        AgentRole::Reviewer | AgentRole::Verifier => {
            if !write_scope.is_empty() {
                return Err(StoreError::InvalidAssignment(
                    "reviewers and verifiers require an empty write scope".to_string(),
                ));
            }
            let expected_kind = if role == AgentRole::Reviewer {
                RelationKind::Review
            } else {
                RelationKind::Verification
            };
            let Some(relation) = relation else {
                return Err(StoreError::InvalidAssignment(format!(
                    "{role:?} requires exactly one {expected_kind:?} target"
                )));
            };
            if relation.kind != expected_kind
                || relation.target_assignment_ids.len() != 1
                || !relation_targets_are_dependencies(relation)
            {
                return Err(StoreError::InvalidAssignment(format!(
                    "{role:?} requires exactly one {expected_kind:?} target that is also a dependency"
                )));
            }
        }
        AgentRole::Integrator => {
            if write_scope.is_empty() {
                return Err(StoreError::InvalidAssignment(
                    "integrators must declare their complete non-empty write scope".to_string(),
                ));
            }
            let Some(relation) = relation else {
                return Err(StoreError::InvalidAssignment(
                    "integrators require an integration relation".to_string(),
                ));
            };
            let unique_targets: HashSet<_> = relation.target_assignment_ids.iter().collect();
            if relation.kind != RelationKind::Integration
                || relation.target_assignment_ids.is_empty()
                || unique_targets.len() != relation.target_assignment_ids.len()
                || !relation_targets_are_dependencies(relation)
            {
                return Err(StoreError::InvalidAssignment(
                    "integrator targets must be non-empty successful dependencies".to_string(),
                ));
            }
        }
    }
    Ok(())
}
