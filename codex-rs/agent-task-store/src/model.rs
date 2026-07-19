use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
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
            Self::Explorer => CapabilityProfile::ReadSearch,
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
    pub relation: Option<AssignmentRelation>,
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
        let mut required_evidence = HashSet::new();
        for requirement in &self.required_evidence {
            validate_nonempty("required evidence", requirement)?;
            if !required_evidence.insert(requirement.as_str()) {
                return Err(StoreError::InvalidAssignment(format!(
                    "duplicate required evidence {requirement}"
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
            relation: self.relation,
            created_at: Utc::now(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Assignment {
    pub assignment_id: AssignmentId,
    pub root_session_id: String,
    /// Stable hash of the canonical repository root. The private absolute root is stored
    /// separately and is never included in task-facing assignment JSON.
    #[serde(default)]
    pub repository_id: String,
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
    pub relation: Option<AssignmentRelation>,
    pub created_at: DateTime<Utc>,
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
pub struct ValidationCall {
    pub call_id: String,
    pub attempt_id: AttemptId,
    pub command_summary: String,
    pub status: ValidationCallStatus,
    pub recorded_at: DateTime<Utc>,
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
    pub sealed_at: DateTime<Utc>,
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
    pub updated_at: DateTime<Utc>,
    pub sealed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WriteClaim {
    pub assignment_id: AssignmentId,
    pub attempt_id: AttemptId,
    pub scopes: Vec<RepoScope>,
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

pub fn evaluate_risk_gate(facts: &RiskFacts) -> RiskGateDecision {
    let mut reasons = Vec::new();
    if facts.configured_high_risk_path {
        reasons.push("configured high-risk contract or path".to_string());
    }
    if facts.cross_owner_scope {
        reasons.push("cross-owner scope".to_string());
    }
    for domain in &facts.domains {
        reasons.push(format!("{domain:?} risk").to_lowercase());
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentTask {
    pub assignment: Assignment,
    pub current_attempt: Attempt,
    pub gates: Vec<AgentGate>,
    pub receipt: Option<AgentReceipt>,
    #[serde(default)]
    pub validation_calls: Vec<ValidationCall>,
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
