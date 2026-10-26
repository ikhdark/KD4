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
pub const MAX_VALIDATION_CALLS_PER_TASK: usize = 100;
pub const DEFAULT_WORKSPACE_LEASE_SECONDS: i64 = 120;
pub const MAX_VALIDATION_LEASE_SECONDS: i64 = 120;
pub const NONPRODUCTIVE_RECOVERY_POLICY_VERSION: u32 = 1;
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AssignmentAdmissionOrigin {
    #[default]
    Typed,
    LegacyMessage {
        parent_assignment_id: Option<AssignmentId>,
    },
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
    /// Omitted legacy policies are claims, never implicitly verified proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_policy: Option<CriterionEvidencePolicy>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionEvidenceRequirement {
    ReportOnly,
    SourceInspection,
    ValidationExecution,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFreshness {
    Historical,
    #[default]
    Current,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionEvidencePolicy {
    pub kind: CriterionEvidenceRequirement,
    /// Exact immutable assignment requirement, not an agent-authored coverage label.
    #[serde(default)]
    pub required_evidence: Option<String>,
    #[serde(default)]
    pub freshness: EvidenceFreshness,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CriterionAssessment {
    pub criterion_id: String,
    pub satisfied: bool,
    pub reason: String,
}

impl AcceptanceCriterion {
    fn validate_evidence_policy(&self) -> StoreResult<()> {
        let Some(policy) = &self.evidence_policy else {
            return Ok(());
        };
        if policy.kind == CriterionEvidenceRequirement::ReportOnly {
            if policy.required_evidence.is_some() {
                return Err(StoreError::InvalidAssignment(
                    "report_only cannot bind a required_evidence entry".into(),
                ));
            }
            return Ok(());
        }
        if policy.kind == CriterionEvidenceRequirement::ValidationExecution
            && policy.freshness == EvidenceFreshness::Current
        {
            return Err(StoreError::InvalidAssignment(
                "current validation_execution requires trusted execution input binding, which is unavailable; another run cannot satisfy it. Historical is valid only for an explicitly historical execution requirement".into(),
            ));
        }
        let requirement = policy
            .required_evidence
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| StoreError::InvalidAssignment(
                "evidence policies require an exact required_evidence binding".into(),
            ))?;
        match policy.kind {
            CriterionEvidenceRequirement::SourceInspection => {
                if !requirement.strip_prefix("inspect:").is_some_and(|path| {
                    !path.is_empty()
                        && path.trim() == path
                        && !path.contains(['\0', '\\', ':'])
                        && !path.starts_with(['/', '~'])
                        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
                }) {
                    return Err(StoreError::InvalidAssignment(
                        "source_inspection requires inspect:<normalized repository-relative file>".into(),
                    ));
                }
            }
            CriterionEvidenceRequirement::ValidationExecution => {
                if requirement.starts_with("inspect:")
                    || required_command_argv(requirement).is_none()
                {
                    return Err(StoreError::InvalidAssignment(
                        "validation_execution requires exact command argv".into(),
                    ));
                }
            }
            CriterionEvidenceRequirement::ReportOnly => unreachable!(),
        }
        Ok(())
    }

    fn validate_evidence_binding(&self, required_evidence: &[String]) -> StoreResult<()> {
        self.validate_evidence_policy()?;
        if let Some(requirement) = self.evidence_policy.as_ref()
            .and_then(|policy| policy.required_evidence.as_ref())
            && !required_evidence.contains(requirement)
        {
            return Err(StoreError::InvalidAssignment(
                "criterion policy must bind an assignment required_evidence entry".into(),
            ));
        }
        Ok(())
    }
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
    #[serde(default)]
    pub admission_origin: AssignmentAdmissionOrigin,
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
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionRejectionReason {
    DuplicateExplorerInvestigation,
    /// Retained for compatibility; admission no longer emits this reason.
    /// Seal-time fingerprints cannot certify the inputs a producer observed.
    ReusableSealedInvestigation,
    IsolatedIntegratorUnavailable,
}

impl std::fmt::Display for AdmissionRejectionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::DuplicateExplorerInvestigation => "duplicate_explorer_investigation",
            Self::ReusableSealedInvestigation => "reusable_sealed_investigation",
            Self::IsolatedIntegratorUnavailable => "isolated_integrator_unavailable",
        };
        formatter.write_str(value)
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationPlan {
    #[default]
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

/// Canonical execution identity shared by assignment admission and producers.
pub fn required_command_argv(requirement: &str) -> Option<Vec<String>> {
    serde_json::from_str::<Vec<String>>(requirement).ok().or_else(|| shlex::split(requirement))
        .filter(|argv| argv.first().is_some_and(|program| !program.trim().is_empty()))
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
            criterion.validate_evidence_binding(&self.required_evidence)?;
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
            admission_origin: self.admission_origin,
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
            integration_plan: IntegrationPlan::SingleWriter,
            // Set by selective admission, which is the only place overlap is computed.
            benign_read_overlap_count: 0,
            task_capsule: None,
            created_at: Utc::now(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Assignment {
    pub assignment_id: AssignmentId,
    pub root_session_id: String,
    #[serde(default)]
    pub admission_origin: AssignmentAdmissionOrigin,
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
    /// Durable integration ownership selected by admission and enforced by the spawn bootstrap.
    #[serde(default)]
    pub integration_plan: IntegrationPlan,
    /// Count of concurrently active assignments whose read scope overlapped this one at
    /// admission. Benign for correctness, but it is the direct signal of duplicated
    /// repository discovery across agents, so it is retained for terminal usage metrics.
    #[serde(default)]
    pub benign_read_overlap_count: u32,
    /// Immutable canonical `TaskCapsuleV1` JSON attached before the child is launched.
    #[serde(default)]
    pub task_capsule: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Assignment {
    pub fn primary_investigation_identity(&self) -> Option<PrimaryInvestigationIdentity> {
        // Dependency IDs alone cannot prove that two producers admitted the
        // same dependency receipt/version. Until those inputs are in the key,
        // dependent investigations must not suppress fresh work.
        if self.role != AgentRole::Explorer || !self.dependencies.is_empty() {
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
        let mut dependencies = self.dependencies.clone();
        dependencies.sort();
        let mut prohibited = self.prohibited_changes.clone();
        prohibited.sort();
        let mut risks = self.risk_hints.clone();
        risks.sort();
        let canonical = serde_json::to_vec(&(
            scopes,
            claims,
            self.objective.trim(),
            criteria,
            dependencies,
            self.stop_condition.trim(),
            prohibited,
            risks,
            &self.relation,
            &self.architecture_contract_ref,
        ))
        .ok()?;
        Some(PrimaryInvestigationIdentity(hex::encode(Sha256::digest(
            &canonical,
        ))))
    }

    pub(crate) fn validate_selective_role_contract(&self) -> StoreResult<()> {
        let invalid = |detail: &str| Err(StoreError::InvalidAssignment(detail.to_string()));
        match self.role {
            AgentRole::Architect => {
                if self.read_scope.is_empty() {
                    return invalid("architects require a non-empty read_scope");
                }
            }
            AgentRole::Explorer => {
                if self.read_scope.is_empty() {
                    return invalid("explorers require a non-empty read_scope");
                }
            }
            AgentRole::Worker => {
                if self.write_scope.is_empty() {
                    return invalid("workers require a non-empty write_scope");
                }
            }
            AgentRole::Reviewer | AgentRole::Verifier | AgentRole::Integrator => {}
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stop_condition: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<AssignmentId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contract_claims: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_strategy: Option<WorkspaceStrategy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<AssignmentRelation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture_contract_ref: Option<ArchitectureContractRef>,
    #[serde(default)]
    pub integration_plan: IntegrationPlan,
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


/// Current specification, distinct from the immutable original assignment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveTaskContract {
    pub objective: String,
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    pub stop_condition: String,
}

impl Assignment {
    pub fn effective_criteria<'a>(
        &'a self,
        amendment: Option<&'a AttemptAmendment>,
    ) -> &'a [AcceptanceCriterion] {
        amendment
            .and_then(|value| value.acceptance_criteria.as_deref())
            .unwrap_or(&self.acceptance_criteria)
    }

    pub fn effective_contract(
        &self,
        amendment: Option<&AttemptAmendment>,
    ) -> EffectiveTaskContract {
        EffectiveTaskContract {
            objective: amendment
                .and_then(|value| value.objective.as_ref())
                .unwrap_or(&self.objective)
                .clone(),
            acceptance_criteria: self.effective_criteria(amendment).to_vec(),
            stop_condition: amendment
                .and_then(|value| value.stop_condition.as_ref())
                .unwrap_or(&self.stop_condition)
                .clone(),
        }
    }
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
                criterion.validate_evidence_policy()?;
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

    pub(crate) fn validate_for_assignment(&self, assignment: &Assignment) -> StoreResult<()> {
        self.validate()?;
        // Include inherited policies from assignments admitted by older runtimes.
        for criterion in assignment.effective_criteria(Some(self)) {
            criterion.validate_evidence_binding(&assignment.required_evidence)?;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskActor {
    /// Trusted home-wide maintenance only; never construct from a model tool.
    Root,
    RootSession(String),
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

impl TaskActor {
    pub(crate) fn require_root(&self) -> StoreResult<()> {
        match self {
            Self::Root | Self::RootSession(_) => Ok(()),
            Self::Attempt(_) => Err(StoreError::RootAuthorityRequired),
        }
    }

    pub(crate) fn require_root_for(&self, root_session_id: &str) -> StoreResult<()> {
        self.require_root()?;
        if let Self::RootSession(root) = self
            && root != root_session_id
        {
            return Err(StoreError::RootSessionMismatch {
                expected: root.clone(),
                requested: root_session_id.to_string(),
            });
        }
        Ok(())
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
    /// Host-only terminal diagnostics; does not modify acceptance or sealed receipts.
    HostCompletion,
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
                | Self::Editing
                | Self::Reviewing
                | Self::Validating
                | Self::Blocked
                | Self::ToolCall
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
    /// Host scheduling metadata, never an authored acceptance claim. Unknown
    /// future codes remain retrievable without inferring a successful result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_stop_cause: Option<String>,
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
    NotExecuted,
    Cancelled,
}

impl ValidationCallStatus {
    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }

    pub fn is_success(self) -> bool {
        self == Self::Succeeded
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationCall {
    pub call_id: String,
    pub attempt_id: AttemptId,
    pub command_summary: String,
    #[serde(default)]
    pub evidence: ValidationEvidence,
    pub status: ValidationCallStatus,
    pub recorded_at: DateTime<Utc>,
}

impl ValidationCall {
    /// Producer-verified evidence kind, never inferred from a criterion's claim.
    pub fn verified_evidence_kind(&self) -> Option<CriterionEvidenceKind> {
        crate::local::verified_evidence_kind(self)
    }
}

/// Host-only preparation for a complete local source read. It is not proof
/// until the producer finishes. It does not verify workspace freshness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInspectionStart {
    /// Canonical acquisition target for recovery; not workspace freshness proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) canonical_path: Option<std::path::PathBuf>,
    pub(crate) attempt_id: AttemptId,
    pub(crate) path: String,
    pub(crate) workspace_id: String,
    pub(crate) epoch: u64,
}

impl SourceInspectionStart {
    pub fn canonical_path(&self) -> Option<&std::path::Path> {
        self.canonical_path.as_deref()
    }
    pub fn matches_attempt(&self, attempt_id: AttemptId) -> bool {
        self.attempt_id == attempt_id
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceInspectionResult {
    pub version: u8,
    pub kind: String,
    pub call_id: String,
    pub path: String,
    pub source_sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationEvidence {
    #[serde(default)]
    pub start_epoch: u64,
    #[serde(default)]
    pub end_epoch: Option<u64>,
    #[serde(default)]
    pub retained_output_ref: Option<String>,
    #[serde(default)]
    pub output_summary: Option<String>,
    #[serde(default)]
    pub validation_result: Option<serde_json::Value>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
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
    /// Narrative evidence remains readable, but does not establish execution proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<CriterionEvidenceRef>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionEvidenceKind {
    /// A successful recorded execution; does not imply a test or runtime boundary passed.
    ValidationExecution,
    /// The native file reader delivered a complete, hash-bound source snapshot
    /// without establishing semantic inspection, correctness, or freshness.
    /// The persisted variant name is retained for compatibility.
    SourceInspection,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionEvidenceRef {
    pub call_id: String,
    pub workspace_id: String,
    pub evidence_epoch: u64,
    pub kind: CriterionEvidenceKind,
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
    /// Legacy seal-time fingerprint, retained for persisted receipt compatibility.
    /// It does not bind producer-observed inputs and is never trusted for reuse.
    /// New receipts leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_scope_fingerprint: Option<String>,
    pub sealed_at: DateTime<Utc>,
}

/// Bounded result of evaluating the generic productivity deadline against owned operations.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProductivitySummary {
    pub active_owned_operation_count: u32,
    pub cancelled_expired_operation_count: u32,
    pub recovery_threshold_seconds: u64,
    pub recovery_policy_version: u32,
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

/// The exact successful prerequisite admitted with an assignment, not a claim
/// that it remains accepted or that its filesystem inputs remain current.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DependencyAdmission {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub receipt_sha256: String,
    pub gates: Vec<AgentGate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTask {
    pub assignment: Assignment,
    /// Empty on legacy records whose admission provenance was not captured.
    #[serde(default)]
    pub dependency_admissions: Vec<DependencyAdmission>,
    /// Current durable prerequisite authority at this read, not filesystem freshness.
    #[serde(default)]
    pub dependency_blockers: Vec<DependencyBlocker>,
    pub current_attempt: Attempt,
    pub gates: Vec<AgentGate>,
    pub receipt: Option<AgentReceipt>,
    #[serde(default)]
    pub validation_calls: Vec<ValidationCall>,
    /// Derived at this read, not durable proof or a reusable freshness receipt.
    #[serde(default)]
    pub criterion_assessments: Vec<CriterionAssessment>,
    #[serde(default)]
    pub workspace_status: WorkspaceTaskStatus,
    #[serde(default)]
    pub isolation_handoff: Option<IsolationHandoff>,
    #[serde(default)]
    pub integration_handoffs: Vec<IsolationHandoff>,
    pub observations: Vec<RuntimeObservation>,
}

impl AgentTask {
    /// Explicit source policies use the host's acquisition applicability at
    /// this read. Other criteria retain their existing reported-result meaning.
    pub fn criterion_is_satisfied(&self, id: &str) -> bool {
        if self.assignment.effective_criteria(self.current_attempt.amendment.as_ref()).iter()
            .find(|criterion| criterion.id == id)
            .and_then(|criterion| criterion.evidence_policy.as_ref())
            .is_some_and(|policy| policy.kind != CriterionEvidenceRequirement::ReportOnly)
        {
            return self.criterion_assessments.iter()
                .any(|assessment| assessment.criterion_id == id && assessment.satisfied);
        }
        self.receipt.as_ref().is_some_and(|receipt| receipt.criterion_results.iter()
            .any(|result| result.criterion_id == id && result.status == CriterionStatus::Passed))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTaskAuthorization {
    pub root_session_id: String,
    pub admission_origin: AssignmentAdmissionOrigin,
    pub current_attempt: Attempt,
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
    #[serde(default)]
    pub status: WakeReadStatus,
    pub reason: Option<ObservationKind>,
    pub updated_agents: Vec<WakeEvent>,
    pub latest_event_id: Option<WakeEventId>,
    /// Events that fell out of the retained wake stream before this read.
    #[serde(default)]
    pub lost_to_retention_count: u64,
    /// Retained events after `latest_event_id` that did not fit in this page.
    #[serde(default)]
    pub remaining_count: u64,
    pub truncated_count: u64,
    pub timed_out: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeReadStatus {
    /// Status was absent from a response serialized by an older producer.
    #[default]
    Unknown,
    NoStream,
    Empty,
    EventsAvailable,
    TimedOut,
    Closed,
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
