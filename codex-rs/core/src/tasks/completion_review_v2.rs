use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use codex_features::Feature;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[path = "source_classification.rs"]
mod source_classification;

use crate::agent::role::apply_role_to_config;
use crate::codex_delegate::run_codex_thread_one_shot;
use crate::compact::MAX_RETAINED_USER_IMAGE_BYTES;
use crate::compact::MAX_RETAINED_USER_IMAGES;
use crate::config::Config;
use crate::config::Constrained;
use crate::context::CompletionReviewRepair;
use crate::context::ContextualUserFragment;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::task_evidence::AtomicReviewTransition;
use crate::task_evidence::COMPLETION_REVIEW_LENSES as REVIEW_LENSES;
use crate::task_evidence::ClassifiedRequirement;
use crate::task_evidence::ClassifiedRequirementRef;
use crate::task_evidence::ClassifiedSource;
use crate::task_evidence::ClassifiedSourceKind;
use crate::task_evidence::CompletionReviewAttemptInput;
use crate::task_evidence::CompletionReviewAttemptKind;
use crate::task_evidence::CompletionReviewCyclePhase;
use crate::task_evidence::CompletionReviewDispositionReceipt;
use crate::task_evidence::CompletionReviewDossier;
use crate::task_evidence::CompletionReviewFindingInput;
use crate::task_evidence::CompletionReviewFindingReceipt;
use crate::task_evidence::LocalSemanticCue;
use crate::task_evidence::LocalSemanticCueKind;
use crate::task_evidence::ManifestGapInput;
use crate::task_evidence::RecordedReviewAttempt;
use crate::task_evidence::RequirementRecord;
use crate::task_evidence::RequirementStatus;
use crate::task_evidence::ReviewLensSelectionFacts;
use crate::task_evidence::SourceClassificationCacheKey;
use crate::task_evidence::SourceLocalClassification;
use crate::task_evidence::SourceLocalClassificationKind;
use crate::task_evidence::SourceMapping;
use crate::task_evidence::SourceMaterialization;
use crate::task_evidence::SourceSpan;
use crate::task_evidence::TaskEvidenceLedger;
use crate::task_evidence::UserSourceAvailability;
use crate::task_evidence::UserSourceKind;
use crate::task_evidence::UserSourceRecord;
use crate::task_evidence::build_repair_baseline;
use crate::task_evidence::repair_baseline_hash;
use crate::task_evidence::sha256_file;
use crate::task_evidence::source_classification_cache_key;
use crate::task_evidence::source_local_classification_is_valid_for_source;
use crate::task_evidence::source_local_classifications_with_manifest_gaps;

const REVIEW_DEADLINE: Duration = Duration::from_secs(90);
const REVIEW_CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
const MAX_RENDERED_REQUEST_TOKENS: usize = 8_999;
const MAX_REVIEW_OUTPUT_TOKENS: usize = 6_000;
const MAX_REVIEW_FINDINGS: usize = 32;
const AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT: usize = 100;

const SOURCE_CLASSIFICATION_MARKER: &str = "KD4_SOURCE_CLASSIFICATION_REQUEST_V1";
const SOURCE_LOCAL_CLASSIFICATION_MARKER: &str = "KD4_SOURCE_LOCAL_CLASSIFICATION_REQUEST_V3";
const SOURCE_RELATIONSHIP_RESOLUTION_MARKER: &str = "KD4_SOURCE_RELATIONSHIP_RESOLUTION_REQUEST_V1";
const REVIEW_REQUEST_MARKER: &str = "KD4_COMPLETION_REVIEW_REQUEST_V2";

const BEHAVIORAL_LENS: &str = "requirements_and_behavioral_compatibility";
const LIFECYCLE_LENS: &str = "lifecycle_and_concurrency";
const PERSISTENCE_LENS: &str = "persistence_filesystem_safety_rollback_and_atomicity";
const SCHEMA_LENS: &str = "schema_protocol_and_generated_representations";
const SECURITY_LENS: &str = "security_and_trust_boundaries";
const PACKAGING_LENS: &str = "platform_configuration_packaging_and_installation";
const PIPELINE_LENS: &str = "pipeline_cache_snapshot_and_artifact_identity";
const VALIDATION_LENS: &str = "validation_quality_and_changed_test_oracle_integrity";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewRiskDomain {
    Concurrency,
    Lifecycle,
    Persistence,
    Migration,
    Rollback,
    AtomicState,
    FilesystemSafety,
    Schema,
    Protocol,
    Security,
    Unsafe,
    Authentication,
    Permission,
    Sandbox,
    TrustBoundary,
    Installation,
    PlatformConfiguration,
    Manifest,
    Packaging,
    Installer,
    Publishing,
    Release,
    Ci,
    Cache,
    SnapshotProduction,
    Generator,
    ArtifactIdentity,
    Validation,
    TestOracle,
}

impl ReviewRiskDomain {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "concurrency" => Self::Concurrency,
            "lifecycle" => Self::Lifecycle,
            "persistence" => Self::Persistence,
            "migration" => Self::Migration,
            "rollback" => Self::Rollback,
            "atomic_state" | "atomic-state" => Self::AtomicState,
            "filesystem_safety" | "filesystem-safety" => Self::FilesystemSafety,
            "schema" => Self::Schema,
            "protocol" => Self::Protocol,
            "security" => Self::Security,
            "unsafe" => Self::Unsafe,
            "authentication" => Self::Authentication,
            "permission" | "permissions" => Self::Permission,
            "sandbox" => Self::Sandbox,
            "trust_boundary" | "trust-boundary" => Self::TrustBoundary,
            "installation" => Self::Installation,
            "platform_configuration" | "platform-configuration" => Self::PlatformConfiguration,
            "manifest" => Self::Manifest,
            "packaging" => Self::Packaging,
            "installer" => Self::Installer,
            "publishing" => Self::Publishing,
            "release" => Self::Release,
            "ci" => Self::Ci,
            "cache" => Self::Cache,
            "snapshot_production" | "snapshot-production" => Self::SnapshotProduction,
            "generator" => Self::Generator,
            "artifact_identity" | "artifact-identity" => Self::ArtifactIdentity,
            "validation" => Self::Validation,
            "test_oracle" | "test-oracle" => Self::TestOracle,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewSurfaceRole {
    Lifecycle,
    Persistence,
    Schema,
    Security,
    Packaging,
    Pipeline,
    Validation,
}

impl ReviewSurfaceRole {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "lifecycle" | "concurrency" => Self::Lifecycle,
            "persistence" | "migration" | "rollback" | "atomic_state" | "filesystem_safety" => {
                Self::Persistence
            }
            "schema" | "protocol" | "generated_representation" => Self::Schema,
            "security" | "unsafe" | "authentication" | "permission" | "sandbox"
            | "trust_boundary" => Self::Security,
            "installation"
            | "platform_configuration"
            | "manifest"
            | "packaging"
            | "installer"
            | "publishing"
            | "release" => Self::Packaging,
            "ci" | "cache" | "snapshot_production" | "generator" | "artifact_identity" => {
                Self::Pipeline
            }
            "test" | "fixture" | "golden" | "snapshot" | "benchmark" | "validator"
            | "test_oracle" => Self::Validation,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedReviewPath(String);

impl ValidatedReviewPath {
    fn parse(value: &str) -> Option<Self> {
        let replaced = value.replace('\\', "/");
        if replaced.is_empty()
            || replaced.starts_with('/')
            || replaced.starts_with("//")
            || replaced.as_bytes().get(1) == Some(&b':')
        {
            return None;
        }
        let mut components = Vec::new();
        for component in replaced.split('/') {
            match component {
                "" | "." => {}
                ".." => return None,
                component => components.push(component.to_ascii_lowercase()),
            }
        }
        (!components.is_empty()).then(|| Self(components.join("/")))
    }
    fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
    fn basename(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ReviewLensSelectionInput {
    risk_domains: Vec<ReviewRiskDomain>,
    hint_paths: Vec<ValidatedReviewPath>,
    task_mutation_paths: Vec<ValidatedReviewPath>,
    child_mutation_paths: Vec<ValidatedReviewPath>,
    plan_edit_paths: Vec<ValidatedReviewPath>,
    plan_runtime_paths: Vec<ValidatedReviewPath>,
    surface_roles: Vec<ReviewSurfaceRole>,
    validation_asset_paths: Vec<ValidatedReviewPath>,
    generated_artifacts: Vec<ValidatedReviewPath>,
    original_finding_lenses: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectedReviewLenses(Vec<&'static str>);

impl SelectedReviewLenses {
    fn as_slice(&self) -> &[&'static str] {
        &self.0
    }
}

fn select_review_lenses(input: &ReviewLensSelectionInput) -> SelectedReviewLenses {
    let mut selected = BTreeSet::from([BEHAVIORAL_LENS]);
    for domain in &input.risk_domains {
        selected.insert(match domain {
            ReviewRiskDomain::Concurrency | ReviewRiskDomain::Lifecycle => LIFECYCLE_LENS,
            ReviewRiskDomain::Persistence
            | ReviewRiskDomain::Migration
            | ReviewRiskDomain::Rollback
            | ReviewRiskDomain::AtomicState
            | ReviewRiskDomain::FilesystemSafety => PERSISTENCE_LENS,
            ReviewRiskDomain::Schema | ReviewRiskDomain::Protocol => SCHEMA_LENS,
            ReviewRiskDomain::Security
            | ReviewRiskDomain::Unsafe
            | ReviewRiskDomain::Authentication
            | ReviewRiskDomain::Permission
            | ReviewRiskDomain::Sandbox
            | ReviewRiskDomain::TrustBoundary => SECURITY_LENS,
            ReviewRiskDomain::Installation
            | ReviewRiskDomain::PlatformConfiguration
            | ReviewRiskDomain::Manifest
            | ReviewRiskDomain::Packaging
            | ReviewRiskDomain::Installer
            | ReviewRiskDomain::Publishing
            | ReviewRiskDomain::Release => PACKAGING_LENS,
            ReviewRiskDomain::Ci
            | ReviewRiskDomain::Cache
            | ReviewRiskDomain::SnapshotProduction
            | ReviewRiskDomain::Generator
            | ReviewRiskDomain::ArtifactIdentity => PIPELINE_LENS,
            ReviewRiskDomain::Validation | ReviewRiskDomain::TestOracle => VALIDATION_LENS,
        });
    }
    for role in &input.surface_roles {
        selected.insert(match role {
            ReviewSurfaceRole::Lifecycle => LIFECYCLE_LENS,
            ReviewSurfaceRole::Persistence => PERSISTENCE_LENS,
            ReviewSurfaceRole::Schema => SCHEMA_LENS,
            ReviewSurfaceRole::Security => SECURITY_LENS,
            ReviewSurfaceRole::Packaging => PACKAGING_LENS,
            ReviewSurfaceRole::Pipeline => PIPELINE_LENS,
            ReviewSurfaceRole::Validation => VALIDATION_LENS,
        });
    }
    if !input.validation_asset_paths.is_empty() {
        selected.insert(VALIDATION_LENS);
    }
    for path in input
        .hint_paths
        .iter()
        .chain(&input.task_mutation_paths)
        .chain(&input.child_mutation_paths)
        .chain(&input.plan_edit_paths)
        .chain(&input.plan_runtime_paths)
        .chain(&input.validation_asset_paths)
    {
        select_lenses_for_path(path, &mut selected);
    }
    if !input.generated_artifacts.is_empty() {
        selected.insert(SCHEMA_LENS);
        selected.insert(PIPELINE_LENS);
        for path in &input.generated_artifacts {
            select_lenses_for_path(path, &mut selected);
        }
    }
    for lens in &input.original_finding_lenses {
        if let Some(canonical) = REVIEW_LENSES.iter().find(|candidate| **candidate == lens) {
            selected.insert(*canonical);
        }
    }
    SelectedReviewLenses(
        REVIEW_LENSES
            .iter()
            .copied()
            .filter(|lens| selected.contains(lens))
            .collect(),
    )
}

fn select_lenses_for_path(path: &ValidatedReviewPath, selected: &mut BTreeSet<&'static str>) {
    let components = path.components().collect::<BTreeSet<_>>();
    let basename = path.basename();
    let extension = basename.rsplit_once('.').map(|(_, extension)| extension);
    if components
        .iter()
        .any(|c| matches!(*c, "lifecycle" | "concurrency" | "threads" | "async"))
    {
        selected.insert(LIFECYCLE_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "persistence" | "storage" | "migrations" | "rollback" | "filesystem"
        )
    }) || matches!(basename, "database.rs" | "storage.rs" | "migration.rs")
    {
        selected.insert(PERSISTENCE_LENS);
    }
    if components
        .iter()
        .any(|c| matches!(*c, "schema" | "schemas" | "protocol" | "generated"))
        || matches!(extension, Some("proto" | "graphql" | "jsonschema"))
    {
        selected.insert(SCHEMA_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "security" | "auth" | "authentication" | "permissions" | "sandbox" | "unsafe"
        )
    }) {
        selected.insert(SECURITY_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "packaging" | "installer" | "installers" | "release" | "publishing"
        )
    }) || matches!(
        basename,
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "setup.py"
            | "requirements.txt"
            | "install.sh"
            | "install.ps1"
            | "install.bat"
            | "installer.rs"
            | "dockerfile"
            | "manifest.json"
    ) {
        selected.insert(PACKAGING_LENS);
    }
    if components
        .iter()
        .any(|c| matches!(*c, "ci" | ".github" | "cache" | "snapshots" | "generators"))
        || matches!(
            basename,
            "cache.rs" | "cache.ts" | "generator.rs" | "generator.ts"
        )
    {
        selected.insert(PIPELINE_LENS);
    }
    if components.iter().any(|c| {
        matches!(
            *c,
            "tests"
                | "test"
                | "fixtures"
                | "goldens"
                | "snapshots"
                | "benches"
                | "benchmarks"
                | "validators"
        )
    }) || matches!(extension, Some("snap"))
        || basename.ends_with("_test.rs")
        || basename.ends_with(".test.ts")
        || basename.ends_with(".test.js")
    {
        selected.insert(VALIDATION_LENS);
    }
}

fn build_review_lens_selection_input(
    dossier: &CompletionReviewDossier,
) -> Option<ReviewLensSelectionInput> {
    fn paths(values: &[String]) -> Option<Vec<ValidatedReviewPath>> {
        values
            .iter()
            .map(|value| ValidatedReviewPath::parse(value))
            .collect()
    }
    let facts = &dossier.review_lens_selection_facts;
    let mut risk_domains = Vec::new();
    let mut hint_paths = Vec::new();
    for hint in &facts.risk_hints {
        if let Some(domain) = ReviewRiskDomain::parse(hint) {
            risk_domains.push(domain);
        } else if let Some(path) = hint.strip_prefix("path:") {
            hint_paths.push(ValidatedReviewPath::parse(path)?);
        }
    }
    let surface_roles = facts
        .surface_roles
        .iter()
        .map(|role| ReviewSurfaceRole::parse(role))
        .collect::<Option<Vec<_>>>()?;
    if dossier
        .original_findings
        .iter()
        .any(|finding| !REVIEW_LENSES.contains(&finding.lens.as_str()))
    {
        return None;
    }
    Some(ReviewLensSelectionInput {
        risk_domains,
        hint_paths,
        task_mutation_paths: paths(&facts.task_mutation_paths)?,
        child_mutation_paths: paths(&facts.child_mutation_paths)?,
        plan_edit_paths: paths(&facts.plan_edit_paths)?,
        plan_runtime_paths: paths(&facts.plan_runtime_paths)?,
        surface_roles,
        validation_asset_paths: paths(&facts.validation_asset_paths)?,
        generated_artifacts: paths(&facts.generated_artifacts)?,
        original_finding_lenses: dossier
            .original_findings
            .iter()
            .map(|finding| finding.lens.clone())
            .collect(),
    })
}

fn original_findings_identity(findings: &[CompletionReviewFindingReceipt]) -> Option<String> {
    let mut canonical = findings.to_vec();
    canonical.sort_by(|left, right| left.finding_id.cmp(&right.finding_id));
    let encoded = serde_json::to_vec(&canonical).ok()?;
    Some(format!("{:x}", Sha256::digest(encoded)))
}

const REVIEWER_BASE_INSTRUCTIONS: &str = r#"You are the independent KD4 completion reviewer. Work read-only. Inspect only the accepted task contract, applicable AGENTS.md and SOURCEMAP.md, owning code, unchanged and changed relevant tests, changed snapshots and fixtures, generated owners, and one-hop callers or consumers. Do not perform a repository-wide audit. Report only a violation of an active requirement, an affected behavioral contract incompatibility, required missing or stale completion evidence, or a defect introduced or exposed by the candidate delta or its one-hop boundaries. Do not report style preferences, unrelated preexisting defects, speculative improvements, broad cleanup, or unreproduced historical findings. Treat changed tests, snapshots, fixtures, and generators as evidence to audit, not authority that can redefine correct behavior. Return only the requested structured JSON."#;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TurnReviewPhase {
    #[default]
    Ready,
    CorrectionInjected,
    Terminal,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionReviewState {
    phase: TurnReviewPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionReviewTurnBaseline {
    implementation_identity_hash: String,
    dossier_snapshot_id: String,
}

#[derive(Debug, Default)]
pub(crate) struct CompletionReviewCoordinatorOutcome {
    pub(crate) repair_injected: bool,
    pub(crate) provisional_clean: bool,
    pub(crate) advisory: Option<String>,
    pub(crate) partial_reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewFailureCategory {
    Timeout,
    Capacity,
    SpawnModel,
    MalformedOutput,
    OversizedOutput,
    OversizedRequest,
    Cleanup,
    Persistence,
    InputUnavailable,
    SourceDrift,
    RepeatedManifestGap,
}

impl ReviewFailureCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Capacity => "capacity",
            Self::SpawnModel => "spawn_model",
            Self::MalformedOutput => "malformed_output",
            Self::OversizedOutput => "oversized_output",
            Self::OversizedRequest => "oversized_request",
            Self::Cleanup => "cleanup",
            Self::Persistence => "persistence",
            Self::InputUnavailable => "input_unavailable_or_truncated",
            Self::SourceDrift => "user_source_drift",
            Self::RepeatedManifestGap => "repeated_or_invalid_manifest_gap",
        }
    }

    const fn partial_reason(self) -> &'static str {
        match self {
            Self::Timeout => "completion reviewer timed out",
            Self::Capacity => "completion reviewer private capacity was unavailable",
            Self::SpawnModel => "completion reviewer could not start or complete",
            Self::MalformedOutput => "completion reviewer returned malformed structured output",
            Self::OversizedOutput => "completion reviewer output exceeded the private output bound",
            Self::OversizedRequest => {
                "completion dossier exceeded the private request bound without truncation"
            }
            Self::Cleanup => "completion reviewer cleanup did not complete",
            Self::Persistence => "completion review state could not be persisted atomically",
            Self::InputUnavailable => "a user source is unavailable or truncated",
            Self::SourceDrift => "a file-backed user source changed after immutable capture",
            Self::RepeatedManifestGap => "a manifest gap could not be reconstructed safely",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClassificationResultKind {
    RequirementBearing,
    NonRequirement,
    SupersededContext,
    UnavailableOrTruncated,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireRequirementStatus {
    Active,
    Superseded,
    Withdrawn,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireSpan {
    kind: String,
    start: usize,
    end: usize,
    reference: String,
    subreference: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClassificationRequirement {
    source_span: WireSpan,
    status: WireRequirementStatus,
    superseded_by_source_id: String,
    superseded_by_span: WireSpan,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceClassificationResult {
    source_id: String,
    result: ClassificationResultKind,
    requirements: Vec<ClassificationRequirement>,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceClassificationOutput {
    sources: Vec<SourceClassificationResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireLocalSemanticCue {
    kind: LocalSemanticCueKind,
    source_span: Option<WireSpan>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceLocalClassificationResult {
    item_id: String,
    local_kind: SourceLocalClassificationKind,
    requirement_spans: Vec<WireSpan>,
    local_semantic_cues: Vec<WireLocalSemanticCue>,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceLocalClassificationOutput {
    items: Vec<SourceLocalClassificationResult>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceRelationshipOutcome {
    None,
    SupersededContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RelationshipResolutionSource {
    source_id: String,
    source_relationship: SourceRelationshipOutcome,
    requirements: Vec<ClassificationRequirement>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RelationshipResolutionOutput {
    sources: Vec<RelationshipResolutionSource>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FindingSeverity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewFinding {
    finding_local_ordinal: u32,
    requirement_ids: Vec<String>,
    lens: String,
    contract_surface: String,
    severity: FindingSeverity,
    concrete_evidence: String,
    smallest_correction: String,
    focused_proof_route: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FindingDisposition {
    Resolved,
    RebuttalAccepted,
    StillPresent,
    InsufficientProof,
    Regressed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewDisposition {
    finding_id: String,
    disposition: FindingDisposition,
    evidence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestGapReviewResult {
    source_id: String,
    omitted_source_spans: Vec<WireSpan>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct UnsatisfiedRequirementReviewResult {
    requirement_id: String,
    evidence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LensObservation {
    lens: String,
    surfaces: Vec<String>,
    evidence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompletionReviewOutput {
    manifest_gaps: Vec<ManifestGapReviewResult>,
    unsatisfied_requirements: Vec<UnsatisfiedRequirementReviewResult>,
    lens_observations: Vec<LensObservation>,
    findings: Vec<ReviewFinding>,
    prior_finding_dispositions: Vec<ReviewDisposition>,
}

#[derive(Debug)]
enum ReviewerPayload {
    Classification(SourceClassificationOutput),
    ClassificationV2(source_classification::SourceClassificationOutputV2),
    LocalClassification(SourceLocalClassificationOutput),
    RelationshipResolution(RelationshipResolutionOutput),
    Review(CompletionReviewOutput),
}

#[derive(Debug)]
struct ReviewerExecution {
    payload: Option<ReviewerPayload>,
    failures: Vec<ReviewFailureCategory>,
}

impl ReviewerExecution {
    fn failed(category: ReviewFailureCategory) -> Self {
        Self {
            payload: None,
            failures: vec![category],
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ReviewerRequestKind {
    Classification,
    ClassificationV2,
    LocalClassification,
    RelationshipResolution,
    InitialReview,
    Rereview,
}

#[derive(Debug)]
struct ValidatedReview {
    review_clean: bool,
    manifest_gaps: Vec<ManifestGapInput>,
    lens_observations: Vec<LensObservation>,
    findings: Vec<CompletionReviewFindingInput>,
    dispositions: Vec<CompletionReviewDispositionReceipt>,
}

fn wire_span_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "kind": { "type": "string", "enum": ["text", "image", "attachment"] },
            "start": { "type": "integer", "minimum": 0 },
            "end": { "type": "integer", "minimum": 0 },
            "reference": { "type": "string" },
            "subreference": { "type": "string" }
        },
        "required": ["kind", "start", "end", "reference", "subreference"]
    })
}

fn source_classification_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "sources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "source_id": { "type": "string" },
                        "result": {
                            "type": "string",
                            "enum": [
                                "requirement_bearing",
                                "non_requirement",
                                "superseded_context",
                                "unavailable_or_truncated"
                            ]
                        },
                        "requirements": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "source_span": wire_span_schema(),
                                    "status": {
                                        "type": "string",
                                        "enum": ["active", "superseded", "withdrawn"]
                                    },
                                    "superseded_by_source_id": { "type": "string" },
                                    "superseded_by_span": wire_span_schema()
                                },
                                "required": [
                                    "source_span",
                                    "status",
                                    "superseded_by_source_id",
                                    "superseded_by_span"
                                ]
                            }
                        },
                        "reason": { "type": "string" }
                    },
                    "required": ["source_id", "result", "requirements", "reason"]
                }
            }
        },
        "required": ["sources"]
    })
}

fn source_local_classification_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "item_id": { "type": "string" },
                        "local_kind": {
                            "type": "string",
                            "enum": [
                                "requirement_bearing",
                                "non_requirement",
                                "relationship_only_context",
                                "unavailable_or_truncated"
                            ]
                        },
                        "requirement_spans": {
                            "type": "array",
                            "items": wire_span_schema()
                        },
                        "local_semantic_cues": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": [
                                            "assertion",
                                            "replacement_intent",
                                            "withdrawal_intent",
                                            "relationship_only_context"
                                        ]
                                    },
                                    "source_span": {
                                        "anyOf": [wire_span_schema(), { "type": "null" }]
                                    }
                                },
                                "required": ["kind", "source_span"]
                            }
                        },
                        "reason": { "type": "string" }
                    },
                    "required": [
                        "item_id",
                        "local_kind",
                        "requirement_spans",
                        "local_semantic_cues",
                        "reason"
                    ]
                }
            }
        },
        "required": ["items"]
    })
}

fn relationship_resolution_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "sources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "source_id": { "type": "string" },
                        "source_relationship": {
                            "type": "string",
                            "enum": ["none", "superseded_context"]
                        },
                        "requirements": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "source_span": wire_span_schema(),
                                    "status": {
                                        "type": "string",
                                        "enum": ["active", "superseded", "withdrawn"]
                                    },
                                    "superseded_by_source_id": { "type": "string" },
                                    "superseded_by_span": wire_span_schema()
                                },
                                "required": [
                                    "source_span",
                                    "status",
                                    "superseded_by_source_id",
                                    "superseded_by_span"
                                ]
                            }
                        }
                    },
                    "required": ["source_id", "source_relationship", "requirements"]
                }
            }
        },
        "required": ["sources"]
    })
}

fn completion_review_output_schema(selected_lenses: &SelectedReviewLenses) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "manifest_gaps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "source_id": { "type": "string" },
                        "omitted_source_spans": {
                            "type": "array",
                            "minItems": 1,
                            "items": wire_span_schema()
                        }
                    },
                    "required": ["source_id", "omitted_source_spans"]
                }
            },
            "unsatisfied_requirements": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "requirement_id": { "type": "string" },
                        "evidence": { "type": "string" }
                    },
                    "required": ["requirement_id", "evidence"]
                }
            },
            "lens_observations": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "lens": { "type": "string", "enum": selected_lenses.as_slice() },
                        "surfaces": {
                            "type": "array",
                            "minItems": 1,
                            "items": { "type": "string" }
                        },
                        "evidence": { "type": "string" }
                    },
                    "required": ["lens", "surfaces", "evidence"]
                }
            },
            "findings": {
                "type": "array",
                "maxItems": MAX_REVIEW_FINDINGS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "finding_local_ordinal": { "type": "integer", "minimum": 1 },
                        "requirement_ids": { "type": "array", "items": { "type": "string" } },
                        "lens": { "type": "string", "enum": selected_lenses.as_slice() },
                        "contract_surface": { "type": "string" },
                        "severity": { "type": "string", "enum": ["critical", "high", "medium", "low"] },
                        "concrete_evidence": { "type": "string" },
                        "smallest_correction": { "type": "string" },
                        "focused_proof_route": { "type": "string" }
                    },
                    "required": [
                        "finding_local_ordinal",
                        "requirement_ids",
                        "lens",
                        "contract_surface",
                        "severity",
                        "concrete_evidence",
                        "smallest_correction",
                        "focused_proof_route"
                    ]
                }
            },
            "prior_finding_dispositions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "finding_id": { "type": "string" },
                        "disposition": {
                            "type": "string",
                            "enum": [
                                "resolved",
                                "rebuttal_accepted",
                                "still_present",
                                "insufficient_proof",
                                "regressed"
                            ]
                        },
                        "evidence": { "type": "string" }
                    },
                    "required": ["finding_id", "disposition", "evidence"]
                }
            }
        },
        "required": [
            "manifest_gaps",
            "unsatisfied_requirements",
            "lens_observations",
            "findings",
            "prior_finding_dispositions"
        ]
    })
}

async fn build_reviewer_config(
    turn_context: &TurnContext,
    requires_images: bool,
) -> Result<Config, ()> {
    let mut config = turn_context.config.as_ref().clone();
    let inherited_model_provider = config.model_provider.clone();
    apply_role_to_config(&mut config, Some("kd4_reviewer"))
        .await
        .map_err(|_| ())?;
    config.model_provider = inherited_model_provider;
    if requires_images {
        config.model = Some(turn_context.model_info.slug.clone());
    }

    config.ephemeral = true;
    config.notify = None;
    config.base_instructions = Some(REVIEWER_BASE_INSTRUCTIONS.to_string());
    config.developer_instructions = None;
    config.personality = None;
    config.include_permissions_instructions = false;
    config.include_apps_instructions = false;
    config.include_collaboration_mode_instructions = false;
    config.include_skill_instructions = false;
    config.include_environment_context = false;
    config.orchestrator_skills_enabled = false;
    config.orchestrator_mcp_enabled = false;
    config.memories.use_memories = false;
    config.memories.dedicated_tools = false;
    config.permissions.approval_policy = Constrained::allow_only(AskForApproval::Never);
    config
        .permissions
        .set_permission_profile(PermissionProfile::read_only())
        .map_err(|_| ())?;
    config
        .web_search_mode
        .set(WebSearchMode::Disabled)
        .map_err(|_| ())?;
    config.mcp_servers.set(HashMap::new()).map_err(|_| ())?;
    for feature in [
        Feature::SpawnCsv,
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::Apps,
        Feature::EnableMcpApps,
        Feature::Plugins,
        Feature::WebSearchRequest,
        Feature::WebSearchCached,
        Feature::CodeMode,
        Feature::CodeModeHost,
        Feature::CodeModeOnly,
        Feature::CodexHooks,
        Feature::Personality,
    ] {
        config.features.disable(feature).map_err(|_| ())?;
        if config.features.enabled(feature) {
            return Err(());
        }
    }
    Ok(config)
}

async fn run_reviewer_with_deadline(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    inputs: Vec<UserInput>,
    kind: ReviewerRequestKind,
    selected_lenses: Option<SelectedReviewLenses>,
    parent_cancellation: &CancellationToken,
) -> CodexResult<ReviewerExecution> {
    let review_cancellation = CancellationToken::new();
    let mut run = Box::pin(run_reviewer_once(
        Arc::clone(sess),
        Arc::clone(turn_context),
        inputs,
        kind,
        selected_lenses,
        review_cancellation.clone(),
    ));
    tokio::select! {
        biased;
        _ = parent_cancellation.cancelled() => {
            review_cancellation.cancel();
            let _ = timeout(REVIEW_CLEANUP_DEADLINE, &mut run).await;
            Err(CodexErr::TurnAborted)
        }
        result = &mut run => Ok(result),
        _ = tokio::time::sleep(REVIEW_DEADLINE) => {
            review_cancellation.cancel();
            let mut execution = ReviewerExecution::failed(ReviewFailureCategory::Timeout);
            if timeout(REVIEW_CLEANUP_DEADLINE, &mut run).await.is_err() {
                execution.failures.push(ReviewFailureCategory::Cleanup);
            }
            Ok(execution)
        }
    }
}

async fn run_reviewer_once(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    inputs: Vec<UserInput>,
    kind: ReviewerRequestKind,
    selected_lenses: Option<SelectedReviewLenses>,
    cancellation_token: CancellationToken,
) -> ReviewerExecution {
    let requires_images = inputs.iter().any(|input| {
        matches!(
            input,
            UserInput::Image { .. } | UserInput::LocalImage { .. }
        )
    });
    let subconfig = match build_reviewer_config(turn_context.as_ref(), requires_images).await {
        Ok(config) => config,
        Err(()) => return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel),
    };
    let schema = match kind {
        ReviewerRequestKind::Classification => source_classification_schema(),
        ReviewerRequestKind::ClassificationV2 => source_classification::v2_schema(),
        ReviewerRequestKind::LocalClassification => source_local_classification_schema(),
        ReviewerRequestKind::RelationshipResolution => relationship_resolution_schema(),
        ReviewerRequestKind::InitialReview | ReviewerRequestKind::Rereview => {
            let Some(selected_lenses) = selected_lenses.as_ref() else {
                return ReviewerExecution::failed(ReviewFailureCategory::InputUnavailable);
            };
            completion_review_output_schema(selected_lenses)
        }
    };
    let io = match run_codex_thread_one_shot(
        subconfig,
        Arc::clone(&sess.services.auth_manager),
        Arc::clone(&sess.services.models_manager),
        inputs,
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        cancellation_token,
        SubAgentSource::Review,
        Some(schema),
        None,
    )
    .await
    {
        Ok(io) => io,
        Err(_) => return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel),
    };
    let termination = io.session_loop_termination.clone();
    let mut reviewer_turn_id = None;
    let raw_output = loop {
        let event = match io.next_event().await {
            Ok(event) => event,
            Err(_) => {
                termination.await;
                return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel);
            }
        };
        match event.msg {
            EventMsg::TurnStarted(started) => {
                reviewer_turn_id.get_or_insert(started.turn_id);
            }
            EventMsg::TurnComplete(completed)
                if reviewer_turn_id.as_deref() == Some(completed.turn_id.as_str()) =>
            {
                break completed.last_agent_message;
            }
            EventMsg::TurnAborted(aborted)
                if reviewer_turn_id.as_deref() == aborted.turn_id.as_deref() =>
            {
                termination.await;
                return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel);
            }
            _ => {}
        }
    };
    termination.await;
    let Some(raw_output) = raw_output else {
        return ReviewerExecution::failed(ReviewFailureCategory::MalformedOutput);
    };
    if approx_token_count(&raw_output) > MAX_REVIEW_OUTPUT_TOKENS {
        return ReviewerExecution::failed(ReviewFailureCategory::OversizedOutput);
    }
    let payload = match kind {
        ReviewerRequestKind::Classification => serde_json::from_str(&raw_output)
            .ok()
            .map(ReviewerPayload::Classification),
        ReviewerRequestKind::ClassificationV2 => serde_json::from_str(&raw_output)
            .ok()
            .map(ReviewerPayload::ClassificationV2),
        ReviewerRequestKind::LocalClassification => serde_json::from_str(&raw_output)
            .ok()
            .map(ReviewerPayload::LocalClassification),
        ReviewerRequestKind::RelationshipResolution => serde_json::from_str(&raw_output)
            .ok()
            .map(ReviewerPayload::RelationshipResolution),
        ReviewerRequestKind::InitialReview | ReviewerRequestKind::Rereview => {
            serde_json::from_str(&raw_output)
                .ok()
                .map(ReviewerPayload::Review)
        }
    };
    match payload {
        Some(payload) => ReviewerExecution {
            payload: Some(payload),
            failures: Vec::new(),
        },
        None => ReviewerExecution::failed(ReviewFailureCategory::MalformedOutput),
    }
}

async fn build_reviewer_inputs(
    dossier: &CompletionReviewDossier,
    kind: ReviewerRequestKind,
    selected_lenses: Option<&SelectedReviewLenses>,
) -> Result<Vec<UserInput>, ReviewFailureCategory> {
    let request = match kind {
        ReviewerRequestKind::Classification => format!(
            "{SOURCE_CLASSIFICATION_MARKER}\n\nClassify every supplied immutable user source exactly once. Split each source into real requirements, non-requirement context, superseded context, or unavailable/truncated content. Requirements must use exact immutable spans. Text spans are UTF-8 byte offsets with 0 <= start < end <= source length; set reference and subreference to empty strings. Image and attachment spans use start=end=0 and copy the supplied source exact_material value into reference; that value is a bounded review reference, while an attached image input supplies image bytes. Use subreference only for a concrete region/range. Active and withdrawn requirements use empty superseded_by fields and an empty text span sentinel (kind=text,start=0,end=0,empty strings). A superseded requirement must point to another requirement span in this same response. Do not infer requirements from model summaries, plans, or tests.\n\n<source_ledger>\n{}\n</source_ledger>",
            classification_dossier_json(dossier)
        ),
        ReviewerRequestKind::InitialReview => {
            let selected_lenses = selected_lenses.ok_or(ReviewFailureCategory::InputUnavailable)?;
            format!(
                "{REVIEW_REQUEST_MARKER}\n\nIndependently review this exact candidate. Return all five required arrays, using empty arrays instead of omitting fields. Report exceptions only: manifest_gaps for real omitted requirements in available immutable source material, unsatisfied_requirements for failed active requirements, lens_observations for material non-blocking notes, findings for newly discovered defects, and an empty prior_finding_dispositions array. Lens observations are strictly advisory and non-blocking. Any failed requirement, missing required proof, actionable defect, or other cleanliness-blocking issue must be emitted through unsatisfied_requirements or findings; never report a blocking issue only as a lens observation. Report any contract-relevant defect using a selected specialized lens when applicable; otherwise use requirements_and_behavioral_compatibility. Manifest-gap spans must use the supplied provenance format. A finding may reference zero or more existing active requirement IDs; a valid cross-cutting compatibility finding may have no requirement ID. The deduplicated set of active requirement IDs referenced by new findings must exactly equal the unsatisfied requirement IDs. Do not return exhaustive satisfied, no-gap, checked-lens, or no-issue attestations. The host validates identity, completeness, contradictions, freshness, and cleanliness.\n\n<completion_dossier>\n{}\n</completion_dossier>",
                review_dossier_json(dossier, false, selected_lenses)
            )
        }
        ReviewerRequestKind::Rereview => {
            let selected_lenses = selected_lenses.ok_or(ReviewFailureCategory::InputUnavailable)?;
            format!(
                "{REVIEW_REQUEST_MARKER}\n\nattempt_kind=rereview\nIndependently rereview the original active requirements, complete frozen original finding set, correction or rebuttal delta represented by the new candidate, changed tests/snapshots/fixtures/generators, and fresh proof receipts. Return all five required arrays, using empty arrays instead of omitting fields. Report exceptions only: manifest_gaps, unsatisfied_requirements, material non-blocking lens_observations, newly discovered findings, and prior_finding_dispositions. Lens observations are strictly advisory and non-blocking. Any failed requirement, missing required proof, actionable defect, or other cleanliness-blocking issue must be emitted through unsatisfied_requirements, findings, or the relevant unresolved prior disposition; never report a blocking issue only as a lens observation. Report any contract-relevant defect using a selected specialized lens when applicable; otherwise use requirements_and_behavioral_compatibility. Disposition every frozen original finding ID exactly once with nonempty evidence and check both that it was fixed or rebutted and that the correction caused no regression. New defects use local finding ordinals and may reference zero or more existing active requirement IDs; a valid cross-cutting compatibility finding may have no requirement ID. The deduplicated unsatisfied active requirement IDs must exactly equal the active requirement IDs referenced by new findings plus frozen original findings dispositioned still_present, insufficient_proof, or regressed. Do not return exhaustive satisfied, no-gap, checked-lens, or no-issue attestations. The host validates identity, completeness, contradictions, freshness, and cleanliness.\n\n<completion_dossier>\n{}\n</completion_dossier>",
                review_dossier_json(dossier, true, selected_lenses)
            )
        }
        ReviewerRequestKind::ClassificationV2 => {
            unreachable!("V2 classification inputs are built from an immutable classification plan")
        }
        ReviewerRequestKind::LocalClassification | ReviewerRequestKind::RelationshipResolution => {
            unreachable!("two-phase source inputs are built from immutable coordinator plans")
        }
    };
    if approx_token_count(&request) > MAX_RENDERED_REQUEST_TOKENS {
        return Err(ReviewFailureCategory::OversizedRequest);
    }

    let mut inputs = vec![UserInput::Text {
        text: request,
        text_elements: Vec::new(),
    }];
    let mut retained_image_count = 0usize;
    let mut retained_image_bytes = 0usize;
    for source in &dossier.sources {
        if source.availability != UserSourceAvailability::Available {
            continue;
        }
        match source.source_kind {
            UserSourceKind::Image => {
                retained_image_count = retained_image_count
                    .checked_add(1)
                    .ok_or(ReviewFailureCategory::OversizedRequest)?;
                if retained_image_count > MAX_RETAINED_USER_IMAGES {
                    return Err(ReviewFailureCategory::OversizedRequest);
                }
                let source_bytes =
                    if let Some(path) = local_image_path_from_material(&source.exact_material) {
                        let file_bytes = tokio::fs::metadata(Path::new(path))
                            .await
                            .map_err(|_| ReviewFailureCategory::SourceDrift)?
                            .len();
                        usize::try_from(file_bytes)
                            .map_err(|_| ReviewFailureCategory::OversizedRequest)?
                    } else {
                        source.exact_material.len()
                    };
                retained_image_bytes = retained_image_bytes
                    .checked_add(source_bytes)
                    .ok_or(ReviewFailureCategory::OversizedRequest)?;
                if retained_image_bytes > MAX_RETAINED_USER_IMAGE_BYTES {
                    return Err(ReviewFailureCategory::OversizedRequest);
                }
                if let Some(path) = local_image_path_from_material(&source.exact_material) {
                    inputs.push(UserInput::LocalImage {
                        path: path.into(),
                        detail: None,
                    });
                } else {
                    inputs.push(UserInput::Image {
                        image_url: source.exact_material.clone(),
                        detail: None,
                    });
                }
            }
            UserSourceKind::Text | UserSourceKind::Attachment => {}
        }
    }
    Ok(inputs)
}

fn classification_dossier_json(dossier: &CompletionReviewDossier) -> String {
    let sources = reviewer_visible_sources(dossier);
    let Ok(serialized) = serde_json::to_string_pretty(&json!({
        "root_task_id": dossier.root_task_id,
        "completion_epoch": dossier.completion_epoch,
        "manifest_revision": dossier.manifest_revision,
        "user_source_ledger_hash": dossier.user_source_ledger_hash,
        "source_capture_failed": dossier.source_capture_failed,
        "sources": sources,
    })) else {
        unreachable!("classification dossier is serializable");
    };
    serialized
}

fn review_dossier_json(
    dossier: &CompletionReviewDossier,
    rereview: bool,
    selected_lenses: &SelectedReviewLenses,
) -> String {
    let sources = reviewer_visible_sources(dossier);
    let requirements = reviewer_visible_requirements(dossier);
    let Ok(serialized) = serde_json::to_string_pretty(&json!({
        "root_task_id": dossier.root_task_id,
        "completion_epoch": dossier.completion_epoch,
        "manifest_revision": dossier.manifest_revision,
        "user_source_ledger_hash": dossier.user_source_ledger_hash,
        "source_capture_failed": dossier.source_capture_failed,
        "requirement_manifest_hash": dossier.requirement_manifest_hash,
        "implementation_identity": dossier.implementation_identity_hash,
        "dossier_snapshot_id": dossier.dossier_snapshot_id,
        "sources": sources,
        "source_mappings": dossier.source_mappings,
        "requirements": requirements,
        "evidence_gate": dossier.evidence_gate,
        "reviewer_visible_evidence": dossier.reviewer_visible_evidence,
        "authoritative_input_errors": dossier.authoritative_input_errors,
        "typed_quiescent": dossier.typed_quiescent,
        "default_children_quiescent": dossier.default_children_quiescent,
        "candidate_completion": dossier.candidate_completion,
        "review_lenses": selected_lenses.as_slice(),
        "rereview": rereview,
        "cycle_parent_review_id": dossier.cycle_parent_review_id,
        "cycle_superseded_review_id": dossier.cycle_superseded_review_id,
        "initial_review_id": dossier.initial_review_id,
        "original_findings": dossier.original_findings,
    })) else {
        unreachable!("review dossier is serializable");
    };
    serialized
}

fn reviewer_source_reference(source: &UserSourceRecord) -> String {
    if source.source_kind == UserSourceKind::Image
        && source
            .exact_material
            .get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
    {
        format!(
            "kd4-source:{}#content-hash={}",
            source.source_id, source.content_hash
        )
    } else {
        source.exact_material.clone()
    }
}

fn reviewer_visible_sources(dossier: &CompletionReviewDossier) -> Vec<UserSourceRecord> {
    dossier
        .sources
        .iter()
        .cloned()
        .map(|mut source| {
            source.exact_material = reviewer_source_reference(&source);
            source
        })
        .collect()
}

fn reviewer_visible_requirements(dossier: &CompletionReviewDossier) -> Vec<RequirementRecord> {
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    dossier
        .requirements
        .iter()
        .cloned()
        .map(|mut requirement| {
            let Some(source) = sources.get(requirement.source_id.as_str()) else {
                return requirement;
            };
            let reference = reviewer_source_reference(source);
            match &mut requirement.source_span {
                SourceSpan::Image {
                    reference: span_reference,
                    region,
                } => {
                    *span_reference = reference.clone();
                    requirement.exact_material =
                        region.as_ref().map_or(reference.clone(), |region| {
                            format!("{reference}#region={region}")
                        });
                }
                SourceSpan::Attachment {
                    reference: span_reference,
                    range,
                } => {
                    *span_reference = reference.clone();
                    requirement.exact_material =
                        range.as_ref().map_or(reference.clone(), |range| {
                            format!("{reference}#range={range}")
                        });
                }
                SourceSpan::Text { .. } => {}
            }
            requirement
        })
        .collect()
}

fn local_image_path_from_material(material: &str) -> Option<&str> {
    let reference = material.strip_prefix("local-image:")?;
    reference.rsplit_once("#sha256=").map(|(path, _hash)| path)
}

fn captured_file_snapshot(source: &UserSourceRecord) -> Result<Option<(&str, &str)>, ()> {
    if source.availability != UserSourceAvailability::Available {
        return Ok(None);
    }
    let path_and_hash = if let Some(reference) = source.exact_material.strip_prefix("local-image:")
    {
        Some(reference)
    } else if let Some(reference) = source.exact_material.strip_prefix("skill:") {
        Some(reference.split_once(':').ok_or(())?.1)
    } else {
        None
    };
    let Some(path_and_hash) = path_and_hash else {
        return Ok(None);
    };
    let (path, expected_hash) = path_and_hash.rsplit_once("#sha256=").ok_or(())?;
    if path.is_empty()
        || expected_hash.len() != 64
        || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(());
    }
    Ok(Some((path, expected_hash)))
}

pub(crate) async fn user_sources_still_current(dossier: &CompletionReviewDossier) -> bool {
    for source in &dossier.sources {
        let snapshot = match captured_file_snapshot(source) {
            Ok(snapshot) => snapshot,
            Err(()) => return false,
        };
        let Some((path, expected_hash)) = snapshot else {
            continue;
        };
        let Ok(observed_hash) = sha256_file(Path::new(path)).await else {
            return false;
        };
        if observed_hash != expected_hash {
            return false;
        }
    }
    true
}

fn wire_span_to_source_span(source: &UserSourceRecord, span: &WireSpan) -> Option<SourceSpan> {
    let reviewer_reference = reviewer_source_reference(source);
    match (source.source_kind, span.kind.as_str()) {
        (UserSourceKind::Text, "text")
            if span.start < span.end
                && span.end <= source.exact_material.len()
                && source.exact_material.is_char_boundary(span.start)
                && source.exact_material.is_char_boundary(span.end)
                && span.reference.is_empty()
                && span.subreference.is_empty() =>
        {
            Some(SourceSpan::Text {
                start: span.start,
                end: span.end,
            })
        }
        (UserSourceKind::Image, "image")
            if span.start == 0 && span.end == 0 && span.reference == reviewer_reference =>
        {
            Some(SourceSpan::Image {
                reference: source.exact_material.clone(),
                region: (!span.subreference.is_empty()).then(|| span.subreference.clone()),
            })
        }
        (UserSourceKind::Attachment, "attachment")
            if span.start == 0 && span.end == 0 && span.reference == reviewer_reference =>
        {
            Some(SourceSpan::Attachment {
                reference: source.exact_material.clone(),
                range: (!span.subreference.is_empty()).then(|| span.subreference.clone()),
            })
        }
        _ => None,
    }
}

fn wire_requirement_status(status: WireRequirementStatus) -> RequirementStatus {
    match status {
        WireRequirementStatus::Active => RequirementStatus::Active,
        WireRequirementStatus::Superseded => RequirementStatus::Superseded,
        WireRequirementStatus::Withdrawn => RequirementStatus::Withdrawn,
    }
}

fn empty_span_sentinel(span: &WireSpan) -> bool {
    span.kind == "text"
        && span.start == 0
        && span.end == 0
        && span.reference.is_empty()
        && span.subreference.is_empty()
}

fn validate_classification(
    dossier: &CompletionReviewDossier,
    output: SourceClassificationOutput,
) -> Option<Vec<ClassifiedSource>> {
    let expected_sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let returned_ids = output
        .sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect::<BTreeSet<_>>();
    if returned_ids.len() != output.sources.len()
        || returned_ids != expected_sources.keys().copied().collect()
    {
        return None;
    }

    let mut converted = Vec::with_capacity(output.sources.len());
    for result in output.sources {
        let source = expected_sources.get(result.source_id.as_str())?;
        if (source.availability == UserSourceAvailability::Available)
            == (result.result == ClassificationResultKind::UnavailableOrTruncated)
        {
            return None;
        }
        let mut requirements = Vec::new();
        for requirement in result.requirements {
            let source_span = wire_span_to_source_span(source, &requirement.source_span)?;
            let status = wire_requirement_status(requirement.status);
            let superseded_by = match status {
                RequirementStatus::Superseded => {
                    if requirement.superseded_by_source_id.is_empty() {
                        return None;
                    }
                    let target =
                        expected_sources.get(requirement.superseded_by_source_id.as_str())?;
                    let target_ref = ClassifiedRequirementRef {
                        source_id: target.source_id.clone(),
                        source_span: wire_span_to_source_span(
                            target,
                            &requirement.superseded_by_span,
                        )?,
                    };
                    if target_ref.source_id == source.source_id
                        && target_ref.source_span == source_span
                    {
                        return None;
                    }
                    Some(target_ref)
                }
                RequirementStatus::Active | RequirementStatus::Withdrawn => {
                    if !requirement.superseded_by_source_id.is_empty()
                        || !empty_span_sentinel(&requirement.superseded_by_span)
                    {
                        return None;
                    }
                    None
                }
            };
            requirements.push(ClassifiedRequirement {
                source_span,
                status,
                superseded_by,
            });
        }
        let (kind, valid_shape) = match result.result {
            ClassificationResultKind::RequirementBearing => (
                ClassifiedSourceKind::RequirementBearing,
                !requirements.is_empty() && result.reason.trim().is_empty(),
            ),
            ClassificationResultKind::NonRequirement => (
                ClassifiedSourceKind::NonRequirement,
                requirements.is_empty() && !result.reason.trim().is_empty(),
            ),
            ClassificationResultKind::SupersededContext => (
                ClassifiedSourceKind::SupersededContext,
                requirements.is_empty() && !result.reason.trim().is_empty(),
            ),
            ClassificationResultKind::UnavailableOrTruncated => (
                ClassifiedSourceKind::UnavailableOrTruncated,
                requirements.is_empty(),
            ),
        };
        if !valid_shape {
            return None;
        }
        converted.push(ClassifiedSource {
            source_id: result.source_id,
            kind,
            requirements,
            reason: (!result.reason.trim().is_empty()).then_some(result.reason),
        });
    }
    Some(converted)
}

#[derive(Clone, Debug)]
struct LocalClassificationMiss {
    item_id: String,
    key: SourceClassificationCacheKey,
    source: UserSourceRecord,
}

#[derive(Clone, Debug)]
struct LocalClassificationPlan {
    local_classifications: BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    misses: Vec<LocalClassificationMiss>,
}

fn plan_local_classification(dossier: &CompletionReviewDossier) -> Option<LocalClassificationPlan> {
    let mut local_classifications = BTreeMap::new();
    let mut misses = Vec::new();
    let mut planned_keys = BTreeSet::new();
    for source in &dossier.sources {
        let key = source_classification_cache_key(source);
        if !planned_keys.insert(key.clone()) {
            continue;
        }
        let matching_sources = dossier
            .sources
            .iter()
            .filter(|candidate| source_classification_cache_key(candidate) == key);
        if let Some(cached) = dossier.source_classification_cache.get(&key)
            && matching_sources
                .clone()
                .all(|candidate| source_local_classification_is_valid_for_source(candidate, cached))
        {
            local_classifications.insert(key, cached.clone());
            continue;
        }
        if source.availability != UserSourceAvailability::Available {
            let local = SourceLocalClassification {
                local_kind: SourceLocalClassificationKind::UnavailableOrTruncated,
                requirement_spans: Vec::new(),
                local_semantic_cues: Vec::new(),
                reason: "source unavailable or truncated".to_string(),
            };
            if !matching_sources
                .clone()
                .all(|candidate| source_local_classification_is_valid_for_source(candidate, &local))
            {
                return None;
            }
            local_classifications.insert(key, local);
            continue;
        }
        misses.push(LocalClassificationMiss {
            item_id: format!("local-source-{}", misses.len() + 1),
            key,
            source: source.clone(),
        });
    }
    Some(LocalClassificationPlan {
        local_classifications,
        misses,
    })
}

async fn build_local_classification_inputs(
    plan: &LocalClassificationPlan,
) -> Result<Vec<UserInput>, ReviewFailureCategory> {
    let items = plan
        .misses
        .iter()
        .map(|miss| {
            json!({
                "item_id": miss.item_id,
                "source_kind": miss.source.source_kind,
                "exact_material": miss.source.exact_material,
            })
        })
        .collect::<Vec<_>>();
    let request = format!(
        "{SOURCE_LOCAL_CLASSIFICATION_MARKER}\n\nClassify every supplied cache-miss item exactly once and in the supplied order. Each item is one immutable source-local classification key, not one relationship occurrence. Inspect only that item's exact material. Return exact requirement spans and source-local semantic cues. Do not assign active, superseded, or withdrawn status; do not compare sources; do not author cross-source relationships. Text spans are UTF-8 byte offsets; image and attachment spans use the supplied immutable reference. reason must be nonempty.\n\n<source_local_items>\n{}\n</source_local_items>",
        serde_json::to_string_pretty(&items)
            .map_err(|_| ReviewFailureCategory::InputUnavailable)?
    );
    if approx_token_count(&request) > MAX_RENDERED_REQUEST_TOKENS {
        return Err(ReviewFailureCategory::OversizedRequest);
    }
    let mut inputs = vec![UserInput::Text {
        text: request,
        text_elements: Vec::new(),
    }];
    let mut retained_image_bytes = 0usize;
    for miss in &plan.misses {
        if miss.source.source_kind != UserSourceKind::Image {
            continue;
        }
        if inputs.len() > MAX_RETAINED_USER_IMAGES {
            return Err(ReviewFailureCategory::OversizedRequest);
        }
        let source_bytes =
            if let Some(path) = local_image_path_from_material(&miss.source.exact_material) {
                usize::try_from(
                    tokio::fs::metadata(Path::new(path))
                        .await
                        .map_err(|_| ReviewFailureCategory::SourceDrift)?
                        .len(),
                )
                .map_err(|_| ReviewFailureCategory::OversizedRequest)?
            } else {
                miss.source.exact_material.len()
            };
        retained_image_bytes = retained_image_bytes
            .checked_add(source_bytes)
            .ok_or(ReviewFailureCategory::OversizedRequest)?;
        if retained_image_bytes > MAX_RETAINED_USER_IMAGE_BYTES {
            return Err(ReviewFailureCategory::OversizedRequest);
        }
        if let Some(path) = local_image_path_from_material(&miss.source.exact_material) {
            inputs.push(UserInput::LocalImage {
                path: path.into(),
                detail: None,
            });
        } else {
            inputs.push(UserInput::Image {
                image_url: miss.source.exact_material.clone(),
                detail: None,
            });
        }
    }
    Ok(inputs)
}

fn validate_local_classification(
    dossier: &CompletionReviewDossier,
    mut plan: LocalClassificationPlan,
    output: SourceLocalClassificationOutput,
) -> Option<BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>> {
    if output.items.len() != plan.misses.len() {
        return None;
    }
    for (returned, miss) in output.items.into_iter().zip(&plan.misses) {
        if returned.item_id != miss.item_id || returned.reason.trim().is_empty() {
            return None;
        }
        let mut requirement_spans = returned
            .requirement_spans
            .iter()
            .map(|span| wire_span_to_source_span(&miss.source, span))
            .collect::<Option<Vec<_>>>()?;
        let requirement_count = requirement_spans.len();
        requirement_spans.sort();
        requirement_spans.dedup();
        if requirement_spans.len() != requirement_count {
            return None;
        }
        let mut local_semantic_cues = returned
            .local_semantic_cues
            .into_iter()
            .map(|cue| {
                Some(LocalSemanticCue {
                    kind: cue.kind,
                    source_span: match cue.source_span.as_ref() {
                        Some(span) => Some(wire_span_to_source_span(&miss.source, span)?),
                        None => None,
                    },
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let cue_count = local_semantic_cues.len();
        local_semantic_cues.sort();
        local_semantic_cues.dedup();
        if local_semantic_cues.len() != cue_count {
            return None;
        }
        let local = SourceLocalClassification {
            local_kind: returned.local_kind,
            requirement_spans,
            local_semantic_cues,
            reason: returned.reason,
        };
        if !dossier
            .sources
            .iter()
            .filter(|source| source_classification_cache_key(source) == miss.key)
            .all(|source| source_local_classification_is_valid_for_source(source, &local))
        {
            return None;
        }
        if plan
            .local_classifications
            .insert(miss.key.clone(), local)
            .is_some()
        {
            return None;
        }
    }
    let expected_keys = dossier
        .sources
        .iter()
        .map(source_classification_cache_key)
        .collect::<BTreeSet<_>>();
    (plan
        .local_classifications
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        == expected_keys)
        .then_some(plan.local_classifications)
}

fn build_relationship_resolution_inputs(
    dossier: &CompletionReviewDossier,
    local_classifications: &BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
) -> Result<Vec<UserInput>, ReviewFailureCategory> {
    let sources = dossier
        .sources
        .iter()
        .map(|source| {
            let local = local_classifications
                .get(&source_classification_cache_key(source))
                .ok_or(ReviewFailureCategory::InputUnavailable)?;
            Ok(json!({
                "source_id": source.source_id,
                "source_ordinal": source.source_ordinal,
                "content_ordinal": source.content_ordinal,
                "local_classification": local,
            }))
        })
        .collect::<Result<Vec<_>, ReviewFailureCategory>>()?;
    let terminal_policy = if dossier.relationship_resolution_current {
        "The recorded relationship resolver version is current. Preserve every existing monotonic terminal status and target exactly; only active requirements may receive a new terminal relationship."
    } else {
        "The recorded relationship resolver version is missing or mismatched. You may correct final statuses and targets, but must preserve every immutable requirement occurrence: source identity, source material/hash, and exact normalized local span."
    };
    let request = format!(
        "{SOURCE_RELATIONSHIP_RESOLUTION_MARKER}\n\nResolve relationships for the complete supplied occurrence list. Return every source exactly once and in order, with one explicit source_relationship value (including none) and every locally classified requirement span exactly once and in local order. This is a non-authoring phase: do not add, remove, split, merge, or alter spans or source-local classifications. Choose only active, superseded, or withdrawn requirement status and, for superseded, one exact target occurrence from the normalized local requirement facts supplied here. Resolve duplicate target material against current source IDs in current ledger order, using source_ordinal and then normalized span as deterministic tie-breakers; cached local facts never select an occurrence. Use source order and local semantic cues. {terminal_policy} Active and withdrawn entries use empty target fields and the empty text span sentinel. source_relationship is superseded_context exactly for relationship-only local context and none otherwise.\n\n<relationship_input>\n{}\n</relationship_input>",
        serde_json::to_string_pretty(&json!({
            "relationship_resolution_current": dossier.relationship_resolution_current,
            "sources": sources,
            "current_requirements": dossier.requirements,
        }))
        .map_err(|_| ReviewFailureCategory::InputUnavailable)?
    );
    if approx_token_count(&request) > MAX_RENDERED_REQUEST_TOKENS {
        return Err(ReviewFailureCategory::OversizedRequest);
    }
    Ok(vec![UserInput::Text {
        text: request,
        text_elements: Vec::new(),
    }])
}

fn validate_relationship_resolution(
    dossier: &CompletionReviewDossier,
    local_classifications: &BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    output: RelationshipResolutionOutput,
) -> Option<Vec<ClassifiedSource>> {
    if output.sources.len() != dossier.sources.len() {
        return None;
    }
    let sources_by_id = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let normalized_requirement_occurrences = dossier
        .sources
        .iter()
        .flat_map(|source| {
            local_classifications
                .get(&source_classification_cache_key(source))
                .into_iter()
                .flat_map(move |local| {
                    local
                        .requirement_spans
                        .iter()
                        .cloned()
                        .map(move |source_span| ClassifiedRequirementRef {
                            source_id: source.source_id.clone(),
                            source_span,
                        })
                })
        })
        .collect::<BTreeSet<_>>();
    let current_requirements_by_occurrence = dossier
        .requirements
        .iter()
        .map(|requirement| {
            (
                ClassifiedRequirementRef {
                    source_id: requirement.source_id.clone(),
                    source_span: requirement.source_span.clone(),
                },
                requirement,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let current_occurrences_by_id = dossier
        .requirements
        .iter()
        .map(|requirement| {
            (
                requirement.requirement_id.as_str(),
                ClassifiedRequirementRef {
                    source_id: requirement.source_id.clone(),
                    source_span: requirement.source_span.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut resolved = Vec::with_capacity(output.sources.len());
    for (result, source) in output.sources.into_iter().zip(&dossier.sources) {
        if result.source_id != source.source_id {
            return None;
        }
        let local = local_classifications.get(&source_classification_cache_key(source))?;
        let expected_relationship = match local.local_kind {
            SourceLocalClassificationKind::RelationshipOnlyContext => {
                SourceRelationshipOutcome::SupersededContext
            }
            _ => SourceRelationshipOutcome::None,
        };
        if result.source_relationship != expected_relationship
            || result.requirements.len() != local.requirement_spans.len()
        {
            return None;
        }
        let mut requirements = Vec::with_capacity(result.requirements.len());
        for (returned, expected_span) in result
            .requirements
            .into_iter()
            .zip(&local.requirement_spans)
        {
            let source_span = wire_span_to_source_span(source, &returned.source_span)?;
            if &source_span != expected_span {
                return None;
            }
            let status = wire_requirement_status(returned.status);
            let superseded_by = match status {
                RequirementStatus::Superseded => {
                    let target = sources_by_id.get(returned.superseded_by_source_id.as_str())?;
                    let target_ref = ClassifiedRequirementRef {
                        source_id: target.source_id.clone(),
                        source_span: wire_span_to_source_span(
                            target,
                            &returned.superseded_by_span,
                        )?,
                    };
                    if target_ref.source_id == source.source_id
                        && target_ref.source_span == source_span
                    {
                        return None;
                    }
                    if !normalized_requirement_occurrences.contains(&target_ref) {
                        return None;
                    }
                    Some(target_ref)
                }
                RequirementStatus::Active | RequirementStatus::Withdrawn => {
                    if !returned.superseded_by_source_id.is_empty()
                        || !empty_span_sentinel(&returned.superseded_by_span)
                    {
                        return None;
                    }
                    None
                }
            };
            let classified = ClassifiedRequirement {
                source_span,
                status,
                superseded_by,
            };
            if dossier.relationship_resolution_current {
                let occurrence = ClassifiedRequirementRef {
                    source_id: source.source_id.clone(),
                    source_span: classified.source_span.clone(),
                };
                if let Some(current) = current_requirements_by_occurrence.get(&occurrence) {
                    match current.status {
                        RequirementStatus::Active => {}
                        RequirementStatus::Withdrawn => {
                            if classified.status != RequirementStatus::Withdrawn
                                || classified.superseded_by.is_some()
                            {
                                return None;
                            }
                        }
                        RequirementStatus::Superseded => {
                            let expected_target = current
                                .superseded_by
                                .as_deref()
                                .and_then(|id| current_occurrences_by_id.get(id));
                            if classified.status != RequirementStatus::Superseded
                                || classified.superseded_by.as_ref() != expected_target
                            {
                                return None;
                            }
                        }
                    }
                }
            }
            requirements.push(classified);
        }
        let kind = match local.local_kind {
            SourceLocalClassificationKind::RequirementBearing => {
                ClassifiedSourceKind::RequirementBearing
            }
            SourceLocalClassificationKind::NonRequirement => ClassifiedSourceKind::NonRequirement,
            SourceLocalClassificationKind::RelationshipOnlyContext => {
                ClassifiedSourceKind::SupersededContext
            }
            SourceLocalClassificationKind::UnavailableOrTruncated => {
                ClassifiedSourceKind::UnavailableOrTruncated
            }
        };
        resolved.push(ClassifiedSource {
            source_id: source.source_id.clone(),
            kind,
            requirements,
            reason: matches!(
                local.local_kind,
                SourceLocalClassificationKind::NonRequirement
                    | SourceLocalClassificationKind::RelationshipOnlyContext
            )
            .then(|| local.reason.clone()),
        });
    }
    Some(resolved)
}

fn source_materialization_from_resolved(
    dossier: &CompletionReviewDossier,
    resolved_sources: Vec<ClassifiedSource>,
) -> Option<SourceMaterialization> {
    if resolved_sources.len() != dossier.sources.len()
        || resolved_sources
            .iter()
            .zip(&dossier.sources)
            .any(|(resolved, source)| resolved.source_id != source.source_id)
    {
        return None;
    }

    let mut local_classifications = BTreeMap::new();
    for (resolved, source) in resolved_sources.iter().zip(&dossier.sources) {
        let mut requirement_spans = resolved
            .requirements
            .iter()
            .map(|requirement| requirement.source_span.clone())
            .collect::<Vec<_>>();
        let requirement_count = requirement_spans.len();
        requirement_spans.sort();
        requirement_spans.dedup();
        if requirement_spans.len() != requirement_count {
            return None;
        }

        let (local_kind, reason) = match resolved.kind {
            ClassifiedSourceKind::RequirementBearing if !requirement_spans.is_empty() => (
                SourceLocalClassificationKind::RequirementBearing,
                "source contains classified requirement spans".to_string(),
            ),
            ClassifiedSourceKind::NonRequirement if requirement_spans.is_empty() => (
                SourceLocalClassificationKind::NonRequirement,
                resolved.reason.clone()?.trim().to_string(),
            ),
            ClassifiedSourceKind::SupersededContext if requirement_spans.is_empty() => (
                SourceLocalClassificationKind::RelationshipOnlyContext,
                resolved.reason.clone()?.trim().to_string(),
            ),
            ClassifiedSourceKind::UnavailableOrTruncated if requirement_spans.is_empty() => (
                SourceLocalClassificationKind::UnavailableOrTruncated,
                "source unavailable or truncated".to_string(),
            ),
            _ => return None,
        };
        if reason.is_empty() {
            return None;
        }

        let mut local_semantic_cues = requirement_spans
            .iter()
            .cloned()
            .map(|source_span| LocalSemanticCue {
                kind: LocalSemanticCueKind::Assertion,
                source_span: Some(source_span),
            })
            .collect::<Vec<_>>();
        if local_kind == SourceLocalClassificationKind::RelationshipOnlyContext {
            local_semantic_cues.push(LocalSemanticCue {
                kind: LocalSemanticCueKind::RelationshipOnlyContext,
                source_span: None,
            });
        }
        local_semantic_cues.sort();
        local_semantic_cues.dedup();

        let local = SourceLocalClassification {
            local_kind,
            requirement_spans,
            local_semantic_cues,
            reason,
        };
        if !source_local_classification_is_valid_for_source(source, &local) {
            return None;
        }
        let key = source_classification_cache_key(source);
        match local_classifications.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(local);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if entry.get() != &local {
                    return None;
                }
            }
        }
    }

    Some(SourceMaterialization {
        local_classifications,
        resolved_sources,
    })
}

async fn materialize_pending_sources(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    dossier: &CompletionReviewDossier,
) -> CodexResult<Result<SourceMaterialization, ReviewFailureCategory>> {
    let Some(route) = source_classification::plan_classification(dossier) else {
        return Ok(Err(ReviewFailureCategory::InputUnavailable));
    };
    let resolved_sources = match route {
        source_classification::ClassificationRoute::LocalOnly(resolved_sources) => {
            if !user_sources_still_current(dossier).await {
                return Ok(Err(ReviewFailureCategory::SourceDrift));
            }
            resolved_sources
        }
        source_classification::ClassificationRoute::V1 => {
            let inputs =
                match build_reviewer_inputs(dossier, ReviewerRequestKind::Classification, None)
                    .await
                {
                    Ok(inputs) => inputs,
                    Err(failure) => return Ok(Err(failure)),
                };
            let execution = match sess.try_acquire_completion_review_slot() {
                Some(_permit) => {
                    run_reviewer_with_deadline(
                        sess,
                        turn_context,
                        inputs,
                        ReviewerRequestKind::Classification,
                        None,
                        cancellation_token,
                    )
                    .await?
                }
                None => ReviewerExecution::failed(ReviewFailureCategory::Capacity),
            };
            if !user_sources_still_current(dossier).await {
                return Ok(Err(ReviewFailureCategory::SourceDrift));
            }
            let Some(ReviewerPayload::Classification(output)) = execution.payload else {
                return Ok(Err(execution
                    .failures
                    .first()
                    .copied()
                    .unwrap_or(ReviewFailureCategory::MalformedOutput)));
            };
            let Some(resolved_sources) = validate_classification(dossier, output) else {
                return Ok(Err(ReviewFailureCategory::MalformedOutput));
            };
            resolved_sources
        }
        source_classification::ClassificationRoute::V2(plan) => {
            let inputs = match source_classification::build_v2_inputs(dossier, &plan).await {
                Ok(inputs) => inputs,
                Err(failure) => return Ok(Err(failure)),
            };
            let execution = match sess.try_acquire_completion_review_slot() {
                Some(_permit) => {
                    run_reviewer_with_deadline(
                        sess,
                        turn_context,
                        inputs,
                        ReviewerRequestKind::ClassificationV2,
                        None,
                        cancellation_token,
                    )
                    .await?
                }
                None => ReviewerExecution::failed(ReviewFailureCategory::Capacity),
            };
            if !user_sources_still_current(dossier).await {
                return Ok(Err(ReviewFailureCategory::SourceDrift));
            }
            let Some(ReviewerPayload::ClassificationV2(output)) = execution.payload else {
                return Ok(Err(execution
                    .failures
                    .first()
                    .copied()
                    .unwrap_or(ReviewFailureCategory::MalformedOutput)));
            };
            let Some(resolved_sources) = source_classification::validate_v2(dossier, &plan, output)
            else {
                return Ok(Err(ReviewFailureCategory::MalformedOutput));
            };
            resolved_sources
        }
    };

    Ok(
        source_materialization_from_resolved(dossier, resolved_sources)
            .ok_or(ReviewFailureCategory::MalformedOutput),
    )
}

async fn materialize_sources(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    dossier: &CompletionReviewDossier,
    seeded_local_classifications: Option<
        BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    >,
) -> CodexResult<Result<SourceMaterialization, ReviewFailureCategory>> {
    if seeded_local_classifications.is_none()
        && dossier
            .source_mappings
            .values()
            .any(|mapping| matches!(mapping, SourceMapping::PendingClassification))
    {
        return materialize_pending_sources(sess, turn_context, cancellation_token, dossier).await;
    }

    let local_classifications = if let Some(local) = seeded_local_classifications {
        local
    } else {
        let Some(plan) = plan_local_classification(dossier) else {
            return Ok(Err(ReviewFailureCategory::InputUnavailable));
        };
        if plan.misses.is_empty() {
            plan.local_classifications
        } else {
            let inputs = match build_local_classification_inputs(&plan).await {
                Ok(inputs) => inputs,
                Err(failure) => return Ok(Err(failure)),
            };
            let execution = match sess.try_acquire_completion_review_slot() {
                Some(_permit) => {
                    run_reviewer_with_deadline(
                        sess,
                        turn_context,
                        inputs,
                        ReviewerRequestKind::LocalClassification,
                        None,
                        cancellation_token,
                    )
                    .await?
                }
                None => ReviewerExecution::failed(ReviewFailureCategory::Capacity),
            };
            if !user_sources_still_current(dossier).await {
                return Ok(Err(ReviewFailureCategory::SourceDrift));
            }
            let Some(ReviewerPayload::LocalClassification(output)) = execution.payload else {
                return Ok(Err(execution
                    .failures
                    .first()
                    .copied()
                    .unwrap_or(ReviewFailureCategory::MalformedOutput)));
            };
            let Some(local) = validate_local_classification(dossier, plan, output) else {
                return Ok(Err(ReviewFailureCategory::MalformedOutput));
            };
            local
        }
    };
    let inputs = match build_relationship_resolution_inputs(dossier, &local_classifications) {
        Ok(inputs) => inputs,
        Err(failure) => return Ok(Err(failure)),
    };
    let execution = match sess.try_acquire_completion_review_slot() {
        Some(_permit) => {
            run_reviewer_with_deadline(
                sess,
                turn_context,
                inputs,
                ReviewerRequestKind::RelationshipResolution,
                None,
                cancellation_token,
            )
            .await?
        }
        None => ReviewerExecution::failed(ReviewFailureCategory::Capacity),
    };
    if !user_sources_still_current(dossier).await {
        return Ok(Err(ReviewFailureCategory::SourceDrift));
    }
    let Some(ReviewerPayload::RelationshipResolution(output)) = execution.payload else {
        return Ok(Err(execution
            .failures
            .first()
            .copied()
            .unwrap_or(ReviewFailureCategory::MalformedOutput)));
    };
    let Some(resolved_sources) =
        validate_relationship_resolution(dossier, &local_classifications, output)
    else {
        return Ok(Err(ReviewFailureCategory::MalformedOutput));
    };
    Ok(Ok(SourceMaterialization {
        local_classifications,
        resolved_sources,
    }))
}

fn validate_review_output(
    dossier: &CompletionReviewDossier,
    output: CompletionReviewOutput,
    rereview: bool,
    selected_lenses: &SelectedReviewLenses,
) -> Option<ValidatedReview> {
    let expected_sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.clone(), source))
        .collect::<BTreeMap<_, _>>();
    let expected_requirements = dossier
        .requirements
        .iter()
        .map(|requirement| (requirement.requirement_id.clone(), requirement))
        .collect::<BTreeMap<_, _>>();
    let host_source_unavailable = dossier.source_capture_failed
        || dossier
            .sources
            .iter()
            .any(|source| source.availability != UserSourceAvailability::Available)
        || !dossier.authoritative_input_errors.is_empty();

    let mut manifest_gaps = Vec::new();
    let mut gap_source_ids = BTreeSet::new();
    for gap in &output.manifest_gaps {
        if !gap_source_ids.insert(gap.source_id.as_str()) || gap.omitted_source_spans.is_empty() {
            return None;
        }
        let source = expected_sources.get(&gap.source_id)?;
        if source.availability != UserSourceAvailability::Available {
            return None;
        }
        if gap
            .omitted_source_spans
            .iter()
            .enumerate()
            .any(|(index, span)| gap.omitted_source_spans[..index].contains(span))
        {
            return None;
        }
        let omitted_spans = gap
            .omitted_source_spans
            .iter()
            .map(|span| wire_span_to_source_span(source, span))
            .collect::<Option<Vec<_>>>()?;
        manifest_gaps.push(ManifestGapInput {
            source_id: gap.source_id.clone(),
            omitted_spans,
        });
    }

    let mut unsatisfied_active_requirement_ids = BTreeSet::<String>::new();
    for unsatisfied in &output.unsatisfied_requirements {
        let expected = expected_requirements.get(&unsatisfied.requirement_id)?;
        if expected.status != RequirementStatus::Active
            || unsatisfied.evidence.trim().is_empty()
            || !unsatisfied_active_requirement_ids.insert(unsatisfied.requirement_id.clone())
        {
            return None;
        }
    }

    let known_lenses = selected_lenses
        .as_slice()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut observed_lenses = BTreeSet::new();
    for observation in &output.lens_observations {
        let unique_surfaces = observation
            .surfaces
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if !known_lenses.contains(observation.lens.as_str())
            || !observed_lenses.insert(observation.lens.as_str())
            || observation.evidence.trim().is_empty()
            || observation.surfaces.is_empty()
            || unique_surfaces.len() != observation.surfaces.len()
            || observation
                .surfaces
                .iter()
                .any(|surface| surface.trim().is_empty())
        {
            return None;
        }
    }

    if output.findings.len() > MAX_REVIEW_FINDINGS {
        return None;
    }
    let expected_ordinals = (1..=output.findings.len() as u32).collect::<Vec<_>>();
    if output
        .findings
        .iter()
        .map(|finding| finding.finding_local_ordinal)
        .collect::<Vec<_>>()
        != expected_ordinals
    {
        return None;
    }
    let mut new_finding_active_requirement_ids = BTreeSet::<String>::new();
    let findings = output
        .findings
        .iter()
        .map(|finding| {
            let referenced_ids = finding.requirement_ids.iter().collect::<BTreeSet<_>>();
            let active_ids = finding
                .requirement_ids
                .iter()
                .filter(|requirement_id| {
                    expected_requirements
                        .get(*requirement_id)
                        .is_some_and(|requirement| requirement.status == RequirementStatus::Active)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            if referenced_ids.len() != finding.requirement_ids.len()
                || active_ids.len() != finding.requirement_ids.len()
                || !known_lenses.contains(finding.lens.as_str())
                || finding.contract_surface.trim().is_empty()
                || finding.concrete_evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.focused_proof_route.trim().is_empty()
            {
                return None;
            }
            new_finding_active_requirement_ids.extend(active_ids);
            Some(CompletionReviewFindingInput {
                local_ordinal: finding.finding_local_ordinal,
                requirement_ids: finding.requirement_ids.clone(),
                lens: finding.lens.clone(),
                contract_surface: finding.contract_surface.clone(),
                severity: match finding.severity {
                    FindingSeverity::Critical => "critical",
                    FindingSeverity::High => "high",
                    FindingSeverity::Medium => "medium",
                    FindingSeverity::Low => "low",
                }
                .to_string(),
                evidence: finding.concrete_evidence.clone(),
                smallest_correction: finding.smallest_correction.clone(),
                proof_route: finding.focused_proof_route.clone(),
            })
        })
        .collect::<Option<Vec<_>>>()?;

    let expected_original_findings = dossier
        .original_findings
        .iter()
        .map(|finding| finding.finding_id.clone())
        .collect::<BTreeSet<_>>();
    let returned_dispositions = output
        .prior_finding_dispositions
        .iter()
        .map(|disposition| disposition.finding_id.clone())
        .collect::<BTreeSet<_>>();
    if (!rereview && !output.prior_finding_dispositions.is_empty())
        || (rereview
            && (returned_dispositions.len() != output.prior_finding_dispositions.len()
                || returned_dispositions != expected_original_findings))
        || output
            .prior_finding_dispositions
            .iter()
            .any(|disposition| disposition.evidence.trim().is_empty())
    {
        return None;
    }
    let dispositions = output
        .prior_finding_dispositions
        .iter()
        .map(|disposition| CompletionReviewDispositionReceipt {
            finding_id: disposition.finding_id.clone(),
            disposition: match disposition.disposition {
                FindingDisposition::Resolved => "resolved",
                FindingDisposition::RebuttalAccepted => "rebuttal_accepted",
                FindingDisposition::StillPresent => "still_present",
                FindingDisposition::InsufficientProof => "insufficient_proof",
                FindingDisposition::Regressed => "regressed",
            }
            .to_string(),
            evidence: disposition.evidence.clone(),
        })
        .collect::<Vec<_>>();

    let unresolved_dispositions = output
        .prior_finding_dispositions
        .iter()
        .filter(|disposition| {
            matches!(
                disposition.disposition,
                FindingDisposition::StillPresent
                    | FindingDisposition::InsufficientProof
                    | FindingDisposition::Regressed
            )
        })
        .collect::<Vec<_>>();
    let original_findings_clean = unresolved_dispositions.is_empty();
    if !rereview {
        if unsatisfied_active_requirement_ids != new_finding_active_requirement_ids {
            return None;
        }
    } else {
        let original_findings_by_id = dossier
            .original_findings
            .iter()
            .map(|finding| (finding.finding_id.as_str(), finding))
            .collect::<BTreeMap<_, _>>();
        let mut unresolved_prior_active_requirement_ids = BTreeSet::<String>::new();
        for disposition in &unresolved_dispositions {
            let original = original_findings_by_id.get(disposition.finding_id.as_str())?;
            let active_ids = original
                .requirement_ids
                .iter()
                .filter(|requirement_id| {
                    expected_requirements
                        .get(*requirement_id)
                        .is_some_and(|requirement| requirement.status == RequirementStatus::Active)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            unresolved_prior_active_requirement_ids.extend(active_ids);
        }
        let effective_unsatisfied_ids = new_finding_active_requirement_ids
            .union(&unresolved_prior_active_requirement_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        if unsatisfied_active_requirement_ids != effective_unsatisfied_ids {
            return None;
        }
    }
    let review_clean = manifest_gaps.is_empty()
        && !host_source_unavailable
        && unsatisfied_active_requirement_ids.is_empty()
        && findings.is_empty()
        && original_findings_clean;
    Some(ValidatedReview {
        review_clean,
        manifest_gaps,
        lens_observations: output.lens_observations,
        findings,
        dispositions,
    })
}

pub(crate) async fn coordinate_completion_review(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    turn_baseline: Option<&CompletionReviewTurnBaseline>,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if state.phase == TurnReviewPhase::Terminal
        || turn_context.session_source.is_non_root_agent()
        || turn_context.collaboration_mode.mode != ModeKind::Default
        || turn_context.final_output_json_schema.is_some()
        || !sess.services.task_evidence.allows_kd4_completion()
    {
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    if !turn_context
        .config
        .features
        .enabled(Feature::TaskCompletionReviewer)
    {
        return Ok(CompletionReviewCoordinatorOutcome {
            advisory: sess.services.task_evidence.finalization_advisory().await,
            ..Default::default()
        });
    }

    let Some(turn_baseline) = turn_baseline else {
        return Ok(CompletionReviewCoordinatorOutcome {
            advisory: sess.services.task_evidence.finalization_advisory().await,
            ..Default::default()
        });
    };
    let Some(eligibility_dossier) = review_dossier(sess, None).await else {
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    };
    let identity_changed = eligibility_dossier.implementation_identity_hash
        != turn_baseline.implementation_identity_hash
        || eligibility_dossier.dossier_snapshot_id != turn_baseline.dossier_snapshot_id;
    let pending_mutating_lineage = eligibility_dossier.has_task_attributed_mutations
        && matches!(
            eligibility_dossier.cycle_phase,
            Some(
                CompletionReviewCyclePhase::ClassificationPending
                    | CompletionReviewCyclePhase::InitialReviewPending
                    | CompletionReviewCyclePhase::CorrectionPending
                    | CompletionReviewCyclePhase::RereviewPending
            )
        );
    if !identity_changed && !pending_mutating_lineage {
        return Ok(CompletionReviewCoordinatorOutcome {
            advisory: sess.services.task_evidence.finalization_advisory().await,
            ..Default::default()
        });
    }

    let Some(mut dossier) = review_dossier(sess, candidate_completion).await else {
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    };
    if matches!(
        dossier.cycle_phase,
        Some(CompletionReviewCyclePhase::TerminalPartial)
    ) {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::RepeatedManifestGap));
    }
    if matches!(
        dossier.cycle_phase,
        Some(CompletionReviewCyclePhase::TerminalBlocked)
    ) {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    if matches!(
        dossier.cycle_phase,
        Some(CompletionReviewCyclePhase::ProvisionalClean)
    ) {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            provisional_clean: true,
            ..Default::default()
        });
    }
    if dossier.active_cycle_id.is_none() {
        match sess
            .services
            .task_evidence
            .begin_completion_review_cycle(&dossier)
            .await
        {
            AtomicReviewTransition::Persisted(_) => {
                let Some(fresh) = review_dossier(sess, candidate_completion).await else {
                    return Ok(partial_outcome(ReviewFailureCategory::Persistence));
                };
                dossier = fresh;
            }
            AtomicReviewTransition::Superseded => {
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
            AtomicReviewTransition::Failed => {
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
    } else {
        match sess
            .services
            .task_evidence
            .begin_completion_review_cycle(&dossier)
            .await
        {
            AtomicReviewTransition::Persisted(_) => {
                let Some(fresh) = review_dossier(sess, candidate_completion).await else {
                    return Ok(partial_outcome(ReviewFailureCategory::Persistence));
                };
                dossier = fresh;
            }
            AtomicReviewTransition::Superseded | AtomicReviewTransition::Failed => {
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
    }

    if dossier.source_capture_failed {
        persist_review_failure(
            sess,
            &dossier,
            CompletionReviewAttemptKind::InitialReview,
            None,
            ReviewFailureCategory::InputUnavailable,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons: vec![
                "a user source could not be durably captured before compaction".to_string(),
            ],
            ..Default::default()
        });
    }
    if !user_sources_still_current(&dossier).await {
        persist_review_failure(
            sess,
            &dossier,
            CompletionReviewAttemptKind::InitialReview,
            None,
            ReviewFailureCategory::SourceDrift,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::SourceDrift));
    }

    if !dossier.mappings_classified {
        let materialization =
            match materialize_sources(sess, turn_context, cancellation_token, &dossier, None)
                .await?
            {
                Ok(materialization) => materialization,
                Err(failure) => {
                    persist_review_failure(
                        sess,
                        &dossier,
                        CompletionReviewAttemptKind::InitialReview,
                        None,
                        failure,
                    )
                    .await;
                    state.phase = TurnReviewPhase::Terminal;
                    return Ok(partial_outcome(failure));
                }
            };
        match sess
            .services
            .task_evidence
            .apply_source_classification(&dossier, materialization)
            .await
        {
            AtomicReviewTransition::Persisted(()) => {
                let Some(fresh) = review_dossier(sess, candidate_completion).await else {
                    return Ok(partial_outcome(ReviewFailureCategory::Persistence));
                };
                dossier = fresh;
            }
            AtomicReviewTransition::Superseded | AtomicReviewTransition::Failed => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
    }

    if dossier.sources.iter().any(|source| {
        source.availability != UserSourceAvailability::Available
            || matches!(
                dossier.source_mappings.get(&source.source_id),
                Some(SourceMapping::UnavailableOrTruncated)
            )
    }) {
        persist_review_failure(
            sess,
            &dossier,
            CompletionReviewAttemptKind::InitialReview,
            None,
            ReviewFailureCategory::InputUnavailable,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::InputUnavailable));
    }

    let kind = match dossier.cycle_phase {
        Some(CompletionReviewCyclePhase::RereviewPending) => ReviewerRequestKind::Rereview,
        Some(CompletionReviewCyclePhase::InitialReviewPending) => {
            ReviewerRequestKind::InitialReview
        }
        Some(CompletionReviewCyclePhase::CorrectionPending) => {
            return resume_correction(
                sess,
                turn_context,
                cancellation_token,
                candidate_completion,
                state,
                dossier,
            )
            .await;
        }
        Some(CompletionReviewCyclePhase::ProvisionalClean) => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome {
                provisional_clean: true,
                ..Default::default()
            });
        }
        Some(CompletionReviewCyclePhase::TerminalBlocked) => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        Some(CompletionReviewCyclePhase::TerminalPartial)
        | Some(CompletionReviewCyclePhase::Closed)
        | Some(CompletionReviewCyclePhase::ClassificationPending)
        | None => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        }
    };
    let mut lens_observation_advisories = Vec::new();
    let mut outcome = run_contract_review(
        sess,
        turn_context,
        cancellation_token,
        candidate_completion,
        state,
        dossier,
        kind,
        false,
        &mut lens_observation_advisories,
    )
    .await?;
    attach_lens_observation_advisories(&mut outcome, lens_observation_advisories);
    Ok(outcome)
}

pub(crate) async fn capture_completion_review_turn_baseline(
    sess: &Session,
) -> Option<CompletionReviewTurnBaseline> {
    if !sess.services.task_evidence.allows_kd4_completion() {
        return None;
    }
    let dossier = review_dossier(sess, None).await?;
    Some(CompletionReviewTurnBaseline {
        implementation_identity_hash: dossier.implementation_identity_hash,
        dossier_snapshot_id: dossier.dossier_snapshot_id,
    })
}

async fn review_dossier(
    sess: &Session,
    candidate_completion: Option<&str>,
) -> Option<CompletionReviewDossier> {
    let authoritative = refresh_authoritative_review_inputs(sess).await;
    sess.services
        .task_evidence
        .completion_review_dossier(
            candidate_completion,
            &authoritative.typed_mutation_identities,
            &authoritative.typed_evidence,
            &authoritative.review_lens_selection_facts,
            &authoritative.partial_reasons,
            authoritative.typed_quiescent,
            authoritative.default_children_quiescent,
        )
        .await
}

pub(crate) async fn implementation_identity_for_evidence(
    sess: &Session,
    ledger: &TaskEvidenceLedger,
) -> Option<String> {
    let authoritative = refresh_authoritative_review_inputs(sess).await;
    ledger
        .completion_review_dossier(
            None,
            &authoritative.typed_mutation_identities,
            &authoritative.typed_evidence,
            &authoritative.review_lens_selection_facts,
            &authoritative.partial_reasons,
            authoritative.typed_quiescent,
            authoritative.default_children_quiescent,
        )
        .await
        .map(|dossier| dossier.implementation_identity_hash)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct AuthoritativeReviewInputs {
    pub(crate) typed_mutation_identities: Vec<String>,
    pub(crate) typed_evidence: Vec<String>,
    pub(crate) partial_reasons: Vec<String>,
    pub(crate) review_lens_selection_facts: ReviewLensSelectionFacts,
    pub(crate) typed_quiescent: bool,
    pub(crate) default_children_quiescent: bool,
}

pub(crate) async fn refresh_authoritative_review_inputs(
    sess: &Session,
) -> AuthoritativeReviewInputs {
    collect_authoritative_review_inputs(sess, true).await
}

pub(crate) async fn inspect_authoritative_review_inputs(
    sess: &Session,
) -> AuthoritativeReviewInputs {
    collect_authoritative_review_inputs(sess, false).await
}

async fn collect_authoritative_review_inputs(
    sess: &Session,
    reconcile_typed_state: bool,
) -> AuthoritativeReviewInputs {
    let (default_children_quiescent, active_default_children) = sess
        .services
        .agent_control
        .default_children_quiescence()
        .await;
    let mut result = AuthoritativeReviewInputs {
        typed_quiescent: true,
        default_children_quiescent,
        ..Default::default()
    };
    if !active_default_children.is_empty() {
        result.typed_evidence.push(format!(
            "default children not quiescent: {}",
            active_default_children.join(", ")
        ));
    }

    let coordinator = sess.services.agent_control.task_coordinator();
    let (Some(store), Some(root_session_id), Some(repo_root)) = (
        coordinator.store(),
        coordinator.root_session_id(),
        sess.services.task_evidence.repository_root(),
    ) else {
        return result;
    };

    let typed_assignment_baseline = sess
        .services
        .task_evidence
        .typed_assignment_baseline()
        .await;
    let bindings_result = store
        .list_agent_task_bindings(root_session_id.clone(), None)
        .await
        .map(|bindings| {
            bindings
                .into_iter()
                .filter(|binding| {
                    !typed_assignment_baseline.contains(&binding.assignment_id.to_string())
                })
                .collect::<Vec<_>>()
        });
    let same_root_typed_actor_ids = bindings_result
        .as_ref()
        .map(|bindings| {
            bindings
                .iter()
                .map(|binding| format!("attempt:{}", binding.attempt_id))
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();

    let event_cursor = sess
        .services
        .task_evidence
        .last_workspace_event_epoch()
        .await;
    match store.read_workspace_events(repo_root, event_cursor).await {
        Ok(events) => {
            if !sess
                .services
                .task_evidence
                .reconcile_default_child_workspace_events(
                    &events,
                    &root_session_id,
                    &same_root_typed_actor_ids,
                )
                .await
            {
                let reason = "default-child workspace evidence could not be persisted".to_string();
                result.typed_evidence.push(reason.clone());
                result.partial_reasons.push(reason);
            }
        }
        Err(error) => {
            let reason = format!("workspace mutation events could not be reconciled: {error}");
            result.typed_evidence.push(reason.clone());
            result.partial_reasons.push(reason);
        }
    }

    if reconcile_typed_state
        && let Err(error) = sess
            .services
            .agent_control
            .reconcile_live_typed_actor_heartbeats()
            .await
    {
        let reason = format!("typed actor liveness reconciliation failed: {error}");
        result.typed_evidence.push(reason.clone());
        result.partial_reasons.push(reason);
    }
    let quiescence = if reconcile_typed_state {
        store.check_quiescence(root_session_id.clone()).await
    } else {
        store.inspect_quiescence(root_session_id.clone()).await
    };
    match quiescence {
        Ok(status) => {
            result.typed_quiescent &= status.quiescent;
            result.typed_evidence.push(
                serde_json::to_string(&status)
                    .unwrap_or_else(|_| "typed quiescence was not serializable".to_string()),
            );
        }
        Err(error) => {
            let reason = format!("typed quiescence failed: {error}");
            result.typed_evidence.push(reason.clone());
            result.partial_reasons.push(reason);
        }
    }

    match bindings_result {
        Ok(mut bindings) => {
            bindings.sort_by_key(|binding| binding.assignment_id);
            for binding in bindings {
                let task = store.get_agent_task(binding.assignment_id, Some(0)).await;
                let mutations = store
                    .list_mutation_evidence(
                        binding.attempt_id,
                        Some(AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT),
                    )
                    .await;
                match (task, mutations) {
                    (Ok(task), Ok(mut mutations)) => {
                        if let Some(reason) = authoritative_mutation_page_saturation_reason(
                            &binding.assignment_id.to_string(),
                            mutations.len(),
                        ) {
                            result.typed_evidence.push(reason.clone());
                            result.partial_reasons.push(reason);
                        }
                        mutations
                            .sort_by_key(|mutation| (mutation.path.clone(), mutation.start_epoch));
                        result
                            .review_lens_selection_facts
                            .child_mutation_paths
                            .extend(mutations.iter().map(|mutation| mutation.path.clone()));
                        result
                            .review_lens_selection_facts
                            .risk_hints
                            .extend(task.assignment.risk_hints.iter().cloned());
                        result.typed_mutation_identities.push(
                            serde_json::to_string(&json!({
                                "assignmentId": binding.assignment_id,
                                "attemptId": binding.attempt_id,
                                "mutations": mutations,
                            }))
                            .unwrap_or_default(),
                        );
                        result.typed_evidence.push(
                            serde_json::to_string(&json!({
                                "binding": binding,
                                "receipt": task.receipt,
                                "gates": task.gates,
                                "validationCalls": task.validation_calls,
                                "workspaceStatus": task.workspace_status,
                            }))
                            .unwrap_or_default(),
                        );
                    }
                    (task, mutations) => {
                        let reason = format!(
                            "typed evidence unavailable for assignment {}: task={:?}; mutations={:?}",
                            binding.assignment_id,
                            task.err(),
                            mutations.err()
                        );
                        result.typed_evidence.push(reason.clone());
                        result.partial_reasons.push(reason);
                    }
                }
            }
        }
        Err(error) => {
            let reason = format!("typed bindings could not be listed: {error}");
            result.typed_evidence.push(reason.clone());
            result.partial_reasons.push(reason);
        }
    }
    result.typed_mutation_identities.sort();
    result.typed_mutation_identities.dedup();
    result.typed_evidence.sort();
    result.typed_evidence.dedup();
    result.partial_reasons.sort();
    result.partial_reasons.dedup();
    result
        .review_lens_selection_facts
        .child_mutation_paths
        .sort();
    result
        .review_lens_selection_facts
        .child_mutation_paths
        .dedup();
    result.review_lens_selection_facts.risk_hints.sort();
    result.review_lens_selection_facts.risk_hints.dedup();
    result
}

fn authoritative_mutation_page_saturation_reason(
    assignment_id: &str,
    mutation_count: usize,
) -> Option<String> {
    (mutation_count == AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT).then(|| {
        format!(
            "typed mutation evidence for assignment {assignment_id} reached the authoritative store page maximum of {AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT}; additional mutation evidence may be omitted"
        )
    })
}

fn partial_outcome(failure: ReviewFailureCategory) -> CompletionReviewCoordinatorOutcome {
    CompletionReviewCoordinatorOutcome {
        partial_reasons: vec![failure.partial_reason().to_string()],
        ..Default::default()
    }
}

fn attach_lens_observation_advisories(
    outcome: &mut CompletionReviewCoordinatorOutcome,
    advisories: Vec<String>,
) {
    if advisories.is_empty() {
        return;
    }
    let observations = advisories.join("\n");
    outcome.advisory = Some(match outcome.advisory.take() {
        Some(existing) => format!("{existing}\n{observations}"),
        None => observations,
    });
}

fn queue_lens_observation_advisories(
    advisories: &mut Vec<String>,
    attempt_kind: CompletionReviewAttemptKind,
    gap_reconstructed: bool,
    review_id: &str,
    parent_review_id: Option<&str>,
    superseded_review_id: Option<&str>,
    observations: &[LensObservation],
) {
    let attempt_kind = if gap_reconstructed {
        "reconstruction"
    } else {
        match attempt_kind {
            CompletionReviewAttemptKind::InitialReview => "initial",
            CompletionReviewAttemptKind::Rereview => "rereview",
            CompletionReviewAttemptKind::CorrectionEvidence
            | CompletionReviewAttemptKind::TerminalClosure => return,
        }
    };
    advisories.extend(observations.iter().map(|observation| {
        json!({
            "type": "completion_review_lens_observation",
            "attempt_kind": attempt_kind,
            "review_id": review_id,
            "parent_review_id": parent_review_id,
            "superseded_review_id": superseded_review_id,
            "lens": observation.lens,
            "surfaces": observation.surfaces,
            "evidence": observation.evidence,
        })
        .to_string()
    }));
}

#[allow(clippy::too_many_arguments)]
async fn run_contract_review(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
    dossier: CompletionReviewDossier,
    kind: ReviewerRequestKind,
    gap_reconstructed: bool,
    lens_observation_advisories: &mut Vec<String>,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    let attempt_kind = match kind {
        ReviewerRequestKind::InitialReview => CompletionReviewAttemptKind::InitialReview,
        ReviewerRequestKind::Rereview => CompletionReviewAttemptKind::Rereview,
        ReviewerRequestKind::Classification
        | ReviewerRequestKind::ClassificationV2
        | ReviewerRequestKind::LocalClassification
        | ReviewerRequestKind::RelationshipResolution => {
            unreachable!()
        }
    };
    let parent_review_id = match kind {
        ReviewerRequestKind::InitialReview => dossier.cycle_parent_review_id.clone(),
        ReviewerRequestKind::Rereview => dossier.initial_review_id.clone(),
        ReviewerRequestKind::Classification
        | ReviewerRequestKind::ClassificationV2
        | ReviewerRequestKind::LocalClassification
        | ReviewerRequestKind::RelationshipResolution => {
            unreachable!()
        }
    };
    let Some(selection_input) = build_review_lens_selection_input(&dossier) else {
        persist_review_failure(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            ReviewFailureCategory::InputUnavailable,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::InputUnavailable));
    };
    let selected_lenses = select_review_lenses(&selection_input);
    let Some(frozen_original_findings_identity) =
        original_findings_identity(&dossier.original_findings)
    else {
        persist_review_failure(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            ReviewFailureCategory::InputUnavailable,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::InputUnavailable));
    };
    if !user_sources_still_current(&dossier).await {
        persist_review_failure(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id.clone(),
            ReviewFailureCategory::SourceDrift,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::SourceDrift));
    }
    let inputs = match build_reviewer_inputs(&dossier, kind, Some(&selected_lenses)).await {
        Ok(inputs) => inputs,
        Err(failure) => {
            persist_review_failure(sess, &dossier, attempt_kind, parent_review_id, failure).await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(failure));
        }
    };
    let execution = match sess.try_acquire_completion_review_slot() {
        Some(_permit) => {
            run_reviewer_with_deadline(
                sess,
                turn_context,
                inputs,
                kind,
                Some(selected_lenses.clone()),
                cancellation_token,
            )
            .await?
        }
        None => ReviewerExecution::failed(ReviewFailureCategory::Capacity),
    };
    if !user_sources_still_current(&dossier).await {
        persist_review_failure(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            ReviewFailureCategory::SourceDrift,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::SourceDrift));
    }
    let Some(ReviewerPayload::Review(output)) = execution.payload else {
        let failure = execution
            .failures
            .first()
            .copied()
            .unwrap_or(ReviewFailureCategory::MalformedOutput);
        persist_review_failure(sess, &dossier, attempt_kind, parent_review_id, failure).await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(failure));
    };
    let Some(validated) = validate_review_output(
        &dossier,
        output,
        matches!(kind, ReviewerRequestKind::Rereview),
        &selected_lenses,
    ) else {
        persist_review_failure(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            ReviewFailureCategory::MalformedOutput,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::MalformedOutput));
    };

    let Some(fresh_dossier) = review_dossier(sess, candidate_completion).await else {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    };
    let refreshed_selection =
        build_review_lens_selection_input(&fresh_dossier).map(|input| select_review_lenses(&input));
    let refreshed_original_findings_identity =
        original_findings_identity(&fresh_dossier.original_findings);
    if fresh_dossier.implementation_identity_hash != dossier.implementation_identity_hash
        || fresh_dossier.dossier_snapshot_id != dossier.dossier_snapshot_id
        || refreshed_original_findings_identity.as_deref()
            != Some(frozen_original_findings_identity.as_str())
        || refreshed_selection.as_ref() != Some(&selected_lenses)
    {
        persist_review_failure(
            sess,
            &fresh_dossier,
            attempt_kind,
            parent_review_id,
            ReviewFailureCategory::Persistence,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons: vec![
                "completion candidate or reviewer-visible evidence changed during review"
                    .to_string(),
            ],
            ..Default::default()
        });
    }
    let dossier = fresh_dossier;

    if !validated.manifest_gaps.is_empty() {
        if gap_reconstructed || dossier.manifest_gap_reconstructed {
            persist_review_failure(
                sess,
                &dossier,
                attempt_kind,
                parent_review_id,
                ReviewFailureCategory::RepeatedManifestGap,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::RepeatedManifestGap));
        }
        let Some(local_classifications) =
            source_local_classifications_with_manifest_gaps(&dossier, &validated.manifest_gaps)
        else {
            persist_review_failure(
                sess,
                &dossier,
                attempt_kind,
                parent_review_id,
                ReviewFailureCategory::MalformedOutput,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::MalformedOutput));
        };
        let source_materialization = match materialize_sources(
            sess,
            turn_context,
            cancellation_token,
            &dossier,
            Some(local_classifications),
        )
        .await?
        {
            Ok(materialization) => materialization,
            Err(failure) => {
                persist_review_failure(sess, &dossier, attempt_kind, parent_review_id, failure)
                    .await;
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(failure));
            }
        };
        match persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            None,
            Some(source_materialization),
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await
        {
            Some(_) => {}
            None => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
        let Some(rebuilt) = review_dossier(sess, candidate_completion).await else {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        };
        return Box::pin(run_contract_review(
            sess,
            turn_context,
            cancellation_token,
            candidate_completion,
            state,
            rebuilt,
            ReviewerRequestKind::InitialReview,
            true,
            lens_observation_advisories,
        ))
        .await;
    }

    let gate_status = dossier.evidence_gate.status;
    if !dossier.typed_quiescent || gate_status == TaskCompletionStatus::Blocked {
        let _ = persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            Some("blocked"),
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    if !dossier.authoritative_input_errors.is_empty() {
        let partial_reasons = dossier.authoritative_input_errors.clone();
        let _ = persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            Some("partial"),
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons,
            ..Default::default()
        });
    }
    if !dossier.default_children_quiescent {
        let _ = persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            Some("partial"),
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons: vec![
                "default child work was still active when completion was reviewed".to_string(),
            ],
            ..Default::default()
        });
    }
    if validated.review_clean && gate_status == TaskCompletionStatus::Passed {
        if persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            None,
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await
        .is_none()
        {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        }
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            provisional_clean: true,
            ..Default::default()
        });
    }
    if validated.review_clean
        && gate_status == TaskCompletionStatus::Partial
        && dossier.locally_obtainable_proof_routes.is_empty()
    {
        let partial_reasons = if dossier.evidence_gate.reasons.is_empty() {
            vec![
                "completion evidence is incomplete and has no locally obtainable proof route"
                    .to_string(),
            ]
        } else {
            dossier.evidence_gate.reasons.clone()
        };
        let _ = persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            Some("partial"),
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons,
            ..Default::default()
        });
    }

    if matches!(kind, ReviewerRequestKind::Rereview) || dossier.correction_consumed {
        let _ = persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            Some("partial"),
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons: vec![
                if matches!(kind, ReviewerRequestKind::Rereview) {
                    "completion rereview did not establish a clean, fully evidenced candidate"
                } else {
                    "completion review found a repairable defect after the automatic correction was consumed"
                }
                .to_string(),
            ],
            ..Default::default()
        });
    }

    let Some(preview_review_id) = sess
        .services
        .task_evidence
        .preview_completion_review_id(&dossier)
        .await
    else {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    };
    let preview_findings = preview_finding_receipts(&preview_review_id, &validated.findings);
    let Some((repair_item, repair_payload)) = build_repair_item(&dossier, &preview_findings) else {
        let _ = persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            Some("partial"),
            None,
            gap_reconstructed,
            lens_observation_advisories,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::OversizedRequest));
    };
    let recorded = match persist_validated_attempt(
        sess,
        &dossier,
        attempt_kind,
        parent_review_id,
        validated,
        Some(repair_payload.clone()),
        None,
        None,
        gap_reconstructed,
        lens_observation_advisories,
    )
    .await
    {
        Some(recorded) => recorded,
        None => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        }
    };
    if recorded.review_id != preview_review_id || recorded.findings != preview_findings {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    }
    sess.record_response_item_and_emit_turn_item(turn_context, repair_item)
        .await;
    state.phase = TurnReviewPhase::CorrectionInjected;
    Ok(CompletionReviewCoordinatorOutcome {
        repair_injected: true,
        ..Default::default()
    })
}

async fn resume_correction(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
    dossier: CompletionReviewDossier,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    if dossier.correction_consumed {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            partial_reasons: vec![
                "automatic completion correction was already consumed for this review cycle"
                    .to_string(),
            ],
            ..Default::default()
        });
    }
    let Some(initial_review_id) = dossier.initial_review_id.clone() else {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    };
    if !persist_correction_evidence(sess, &dossier, &initial_review_id).await {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    }
    let Some(after_correction) = review_dossier(sess, candidate_completion).await else {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    };
    if after_correction.cycle_phase != Some(CompletionReviewCyclePhase::RereviewPending) {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(partial_outcome(ReviewFailureCategory::Persistence));
    }
    let mut lens_observation_advisories = Vec::new();
    let mut outcome = run_contract_review(
        sess,
        turn_context,
        cancellation_token,
        candidate_completion,
        state,
        after_correction,
        ReviewerRequestKind::Rereview,
        false,
        &mut lens_observation_advisories,
    )
    .await?;
    attach_lens_observation_advisories(&mut outcome, lens_observation_advisories);
    Ok(outcome)
}

// Persistence records each independently validated review component without an intermediate bag.
#[allow(clippy::too_many_arguments)]
async fn persist_validated_attempt(
    sess: &Session,
    dossier: &CompletionReviewDossier,
    attempt_kind: CompletionReviewAttemptKind,
    parent_review_id: Option<String>,
    validated: ValidatedReview,
    repair_instruction: Option<String>,
    terminal_outcome: Option<&str>,
    source_materialization: Option<SourceMaterialization>,
    gap_reconstructed: bool,
    lens_observation_advisories: &mut Vec<String>,
) -> Option<RecordedReviewAttempt> {
    let advisory_parent_review_id = parent_review_id.clone();
    let superseded_review_id = (attempt_kind == CompletionReviewAttemptKind::InitialReview)
        .then(|| dossier.cycle_superseded_review_id.clone())
        .flatten();
    let ValidatedReview {
        manifest_gaps,
        lens_observations,
        findings,
        dispositions,
        review_clean,
    } = validated;
    let input = CompletionReviewAttemptInput {
        attempt_kind,
        parent_review_id,
        superseded_review_id: superseded_review_id.clone(),
        findings,
        dispositions,
        manifest_gaps,
        repair_instruction,
        repair_instruction_hash: (attempt_kind == CompletionReviewAttemptKind::Rereview)
            .then(|| dossier.initial_repair_instruction_hash.clone())
            .flatten(),
        infrastructure_outcome: "ok".to_string(),
        review_clean,
        terminal_outcome: terminal_outcome.map(str::to_string),
    };
    let transition = if input.manifest_gaps.is_empty() {
        if source_materialization.is_some() {
            return None;
        }
        sess.services
            .task_evidence
            .record_completion_review_attempt_v2(dossier, input)
            .await
    } else {
        sess.services
            .task_evidence
            .record_completion_review_attempt_v2_with_materialization(
                dossier,
                input,
                source_materialization?,
            )
            .await
    };
    match transition {
        AtomicReviewTransition::Persisted(recorded) => {
            queue_lens_observation_advisories(
                lens_observation_advisories,
                attempt_kind,
                gap_reconstructed,
                &recorded.review_id,
                advisory_parent_review_id.as_deref(),
                superseded_review_id.as_deref(),
                &lens_observations,
            );
            Some(recorded)
        }
        AtomicReviewTransition::Superseded | AtomicReviewTransition::Failed => None,
    }
}

async fn persist_review_failure(
    sess: &Session,
    dossier: &CompletionReviewDossier,
    attempt_kind: CompletionReviewAttemptKind,
    parent_review_id: Option<String>,
    failure: ReviewFailureCategory,
) {
    let _ = sess
        .services
        .task_evidence
        .record_completion_review_attempt_v2(
            dossier,
            CompletionReviewAttemptInput {
                attempt_kind,
                parent_review_id,
                superseded_review_id: (attempt_kind == CompletionReviewAttemptKind::InitialReview)
                    .then(|| dossier.cycle_superseded_review_id.clone())
                    .flatten(),
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction: None,
                repair_instruction_hash: (attempt_kind
                    == CompletionReviewAttemptKind::CorrectionEvidence)
                    .then(|| dossier.initial_repair_instruction_hash.clone())
                    .flatten(),
                infrastructure_outcome: failure.as_str().to_string(),
                review_clean: false,
                terminal_outcome: Some("partial".to_string()),
            },
        )
        .await;
}

async fn persist_correction_evidence(
    sess: &Session,
    dossier: &CompletionReviewDossier,
    initial_review_id: &str,
) -> bool {
    matches!(
        sess.services
            .task_evidence
            .record_completion_review_attempt_v2(
                dossier,
                CompletionReviewAttemptInput {
                    attempt_kind: CompletionReviewAttemptKind::CorrectionEvidence,
                    parent_review_id: Some(initial_review_id.to_string()),
                    superseded_review_id: None,
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction: None,
                    repair_instruction_hash: dossier.initial_repair_instruction_hash.clone(),
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: None,
                },
            )
            .await,
        AtomicReviewTransition::Persisted(_)
    )
}

fn preview_finding_receipts(
    review_id: &str,
    findings: &[CompletionReviewFindingInput],
) -> Vec<CompletionReviewFindingReceipt> {
    findings
        .iter()
        .map(|finding| CompletionReviewFindingReceipt {
            finding_id: format!("{review_id}/F{}", finding.local_ordinal),
            requirement_ids: finding.requirement_ids.clone(),
            lens: finding.lens.clone(),
            contract_surface: finding.contract_surface.clone(),
            severity: finding.severity.clone(),
            evidence: finding.evidence.clone(),
            smallest_correction: finding.smallest_correction.clone(),
            proof_route: finding.proof_route.clone(),
        })
        .collect()
}

fn build_repair_item(
    dossier: &CompletionReviewDossier,
    findings: &[CompletionReviewFindingReceipt],
) -> Option<(codex_protocol::models::ResponseItem, String)> {
    if findings.is_empty() && dossier.locally_obtainable_proof_routes.is_empty() {
        return None;
    }
    let active_requirements = reviewer_visible_requirements(dossier)
        .into_iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .collect::<Vec<_>>();
    let repair_baseline = build_repair_baseline(dossier, findings).ok()?;
    let repair_baseline_hash = repair_baseline_hash(&repair_baseline);
    let payload = serde_json::to_string_pretty(&json!({
        "contract": "KD4_COMPLETION_CORRECTION_V2",
        "root_task_id": dossier.root_task_id,
        "completion_epoch": dossier.completion_epoch,
        "manifest_revision": dossier.manifest_revision,
        "implementation_identity": dossier.implementation_identity_hash,
        "reviewed_dossier_snapshot_id": dossier.dossier_snapshot_id,
        "active_requirements": active_requirements,
        "complete_finding_set": findings,
        "repair_baseline_hash": repair_baseline_hash,
        "declared_repair_scope": repair_baseline.repair_scope,
        "applicable_proof_routes": dossier.locally_obtainable_proof_routes,
        "preserved_invariants": [
            "Do not alter immutable user sources or the active requirement manifest.",
            "Do not alter original finding contents or IDs.",
            "Do not change evidence-gate rules or broaden the accepted scope.",
            "Address the complete finding set in this one correction phase."
        ],
        "evidence_gate": dossier.evidence_gate,
        "reviewer_visible_evidence": dossier.reviewer_visible_evidence,
    }))
    .ok()?;
    let item = ContextualUserFragment::into(CompletionReviewRepair::new(payload.clone()));
    if approx_token_count(&serde_json::to_string(&item).ok()?) > MAX_RENDERED_REQUEST_TOKENS {
        return None;
    }
    Some((item, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_evidence::CurrentRepairSnapshot;
    use codex_protocol::protocol::TaskCompletionGate;
    use sha2::Digest;
    use sha2::Sha256;

    fn text_span(start: usize, end: usize) -> WireSpan {
        WireSpan {
            kind: "text".to_string(),
            start,
            end,
            reference: String::new(),
            subreference: String::new(),
        }
    }

    fn empty_span() -> WireSpan {
        text_span(0, 0)
    }

    fn dossier() -> CompletionReviewDossier {
        let source = UserSourceRecord {
            source_id: "source-1".to_string(),
            message_id: "message-1".to_string(),
            source_kind: UserSourceKind::Text,
            content_hash: "source-hash".to_string(),
            source_ordinal: 1,
            content_ordinal: 0,
            exact_material: "implement alpha and beta".to_string(),
            availability: UserSourceAvailability::Available,
            completion_epoch: 1,
            introduced_manifest_revision: 1,
        };
        let requirement = RequirementRecord {
            requirement_id: "requirement-1".to_string(),
            source_id: source.source_id.clone(),
            source_content_hash: source.content_hash.clone(),
            source_span: SourceSpan::Text { start: 0, end: 15 },
            exact_material: "implement alpha".to_string(),
            status: RequirementStatus::Active,
            superseded_by: None,
        };
        let source_classification_cache = BTreeMap::from([(
            source_classification_cache_key(&source),
            SourceLocalClassification {
                local_kind: SourceLocalClassificationKind::RequirementBearing,
                requirement_spans: vec![requirement.source_span.clone()],
                local_semantic_cues: Vec::new(),
                reason: "source-local requirement".to_string(),
            },
        )]);
        CompletionReviewDossier {
            document_revision: 7,
            root_task_id: "root-task".to_string(),
            completion_epoch: 1,
            manifest_revision: 1,
            sources: vec![source],
            source_mappings: BTreeMap::from([(
                "source-1".to_string(),
                SourceMapping::RequirementBearing {
                    requirement_ids: vec!["requirement-1".to_string()],
                },
            )]),
            source_classification_cache,
            source_classification_current: true,
            relationship_resolution_current: true,
            mappings_classified: true,
            source_capture_failed: false,
            requirements: vec![requirement],
            user_source_ledger_hash: "source-ledger-hash".to_string(),
            requirement_manifest_hash: "manifest-hash".to_string(),
            implementation_identity_hash: "implementation-hash".to_string(),
            dossier_snapshot_id: "dossier-hash".to_string(),
            host_mutation_revision: 3,
            has_task_attributed_mutations: true,
            evidence_gate: TaskCompletionGate {
                status: TaskCompletionStatus::Passed,
                reasons: Vec::new(),
                evidence_path: None,
            },
            locally_obtainable_proof_routes: Vec::new(),
            reviewer_visible_evidence: json!({"proof": "focused"}),
            review_lens_selection_facts: ReviewLensSelectionFacts::default(),
            authoritative_input_errors: Vec::new(),
            typed_quiescent: true,
            default_children_quiescent: true,
            candidate_completion: Some("done".to_string()),
            correction_consumed: false,
            cycle_phase: Some(CompletionReviewCyclePhase::InitialReviewPending),
            active_cycle_id: Some("cycle-1".to_string()),
            cycle_parent_review_id: None,
            cycle_superseded_review_id: None,
            accepted_review_id: None,
            initial_review_id: None,
            initial_repair_instruction_hash: None,
            original_findings: Vec::new(),
            manifest_gap_reconstructed: false,
            current_repair_snapshot: CurrentRepairSnapshot {
                repository_root: String::new(),
                path_states: Vec::new(),
                command_receipts: Vec::new(),
                plan_structure_hash: String::new(),
                declared_path_scopes: Vec::new(),
                implementation_surfaces: Vec::new(),
                default_child_mutation_identities: Vec::new(),
                typed_mutation_identities: Vec::new(),
                external_evidence_ids: Vec::new(),
                containment_errors: Vec::new(),
            },
            initial_repair_baseline: None,
            initial_repair_baseline_hash: None,
            rereview_input: None,
        }
    }

    fn selected_lenses(dossier: &CompletionReviewDossier) -> SelectedReviewLenses {
        select_review_lenses(
            &build_review_lens_selection_input(dossier).expect("valid selection input"),
        )
    }

    #[test]
    fn relationship_resolver_contract_uses_occurrence_order_not_cached_local_identity() {
        let mut review_dossier = dossier();
        let current_inputs = build_relationship_resolution_inputs(
            &review_dossier,
            &review_dossier.source_classification_cache,
        )
        .expect("current resolver input");
        let [UserInput::Text { text: current, .. }] = current_inputs.as_slice() else {
            panic!("relationship resolver must emit exactly one text input");
        };
        assert!(current.contains(
            "current source IDs in current ledger order, using source_ordinal and then normalized span as deterministic tie-breakers; cached local facts never select an occurrence"
        ));
        assert!(current.contains(
            "Return every source exactly once and in order, with one explicit source_relationship value (including none)"
        ));
        assert!(
            current
                .contains("Preserve every existing monotonic terminal status and target exactly")
        );

        review_dossier.relationship_resolution_current = false;
        let stale_inputs = build_relationship_resolution_inputs(
            &review_dossier,
            &review_dossier.source_classification_cache,
        )
        .expect("stale resolver input");
        let [UserInput::Text { text: stale, .. }] = stale_inputs.as_slice() else {
            panic!("relationship resolver must emit exactly one text input");
        };
        assert!(stale.contains(
            "You may correct final statuses and targets, but must preserve every immutable requirement occurrence"
        ));
    }

    #[test]
    fn local_classification_plan_groups_unique_misses_and_reuses_hits_for_resolver_transition() {
        let mut review_dossier = dossier();
        let cached_local = review_dossier
            .source_classification_cache
            .values()
            .next()
            .expect("valid cached local projection")
            .clone();
        review_dossier.sources[0].content_hash = "a".repeat(64);
        let mut duplicate = review_dossier.sources[0].clone();
        duplicate.source_id = "source-2".to_string();
        duplicate.message_id = "message-2".to_string();
        duplicate.source_ordinal = 2;
        review_dossier.sources.push(duplicate);

        review_dossier.source_classification_cache.clear();
        review_dossier.source_classification_current = false;
        review_dossier.mappings_classified = false;
        let source_transition =
            plan_local_classification(&review_dossier).expect("source transition plan");
        assert_eq!(source_transition.misses.len(), 1);
        assert_eq!(source_transition.misses[0].item_id, "local-source-1");
        assert!(source_transition.local_classifications.is_empty());

        let key = source_classification_cache_key(&review_dossier.sources[0]);
        review_dossier
            .source_classification_cache
            .insert(key.clone(), cached_local);
        review_dossier.source_classification_current = true;
        review_dossier.relationship_resolution_current = false;
        let resolver_transition =
            plan_local_classification(&review_dossier).expect("resolver transition plan");
        assert!(resolver_transition.misses.is_empty());
        assert_eq!(
            resolver_transition
                .local_classifications
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![key]
        );
    }

    fn validate(
        dossier: &CompletionReviewDossier,
        output: CompletionReviewOutput,
        rereview: bool,
    ) -> Option<ValidatedReview> {
        validate_review_output(dossier, output, rereview, &selected_lenses(dossier))
    }

    fn clean_output() -> CompletionReviewOutput {
        CompletionReviewOutput {
            manifest_gaps: Vec::new(),
            unsatisfied_requirements: Vec::new(),
            lens_observations: Vec::new(),
            findings: Vec::new(),
            prior_finding_dispositions: Vec::new(),
        }
    }

    fn valid_finding() -> ReviewFinding {
        ReviewFinding {
            finding_local_ordinal: 1,
            requirement_ids: vec!["requirement-1".to_string()],
            lens: BEHAVIORAL_LENS.to_string(),
            contract_surface: "bounded owner".to_string(),
            severity: FindingSeverity::High,
            concrete_evidence: "the active requirement is not met".to_string(),
            smallest_correction: "implement the missing branch".to_string(),
            focused_proof_route: "cargo test focused_case".to_string(),
        }
    }

    #[test]
    fn classification_requires_exact_source_coverage_and_valid_shapes() {
        let dossier = dossier();
        let valid = SourceClassificationOutput {
            sources: vec![SourceClassificationResult {
                source_id: "source-1".to_string(),
                result: ClassificationResultKind::RequirementBearing,
                requirements: vec![ClassificationRequirement {
                    source_span: text_span(0, 15),
                    status: WireRequirementStatus::Active,
                    superseded_by_source_id: String::new(),
                    superseded_by_span: empty_span(),
                }],
                reason: String::new(),
            }],
        };
        assert!(validate_classification(&dossier, valid.clone()).is_some());

        let mut missing = valid.clone();
        missing.sources.clear();
        assert!(validate_classification(&dossier, missing).is_none());
        let mut duplicate = valid.clone();
        duplicate.sources.push(valid.sources[0].clone());
        assert!(validate_classification(&dossier, duplicate).is_none());
        let mut empty_requirement = valid.clone();
        empty_requirement.sources[0].requirements.clear();
        assert!(validate_classification(&dossier, empty_requirement).is_none());
        let mut false_reason = valid;
        false_reason.sources[0].reason = "not a requirement".to_string();
        assert!(validate_classification(&dossier, false_reason).is_none());
    }

    #[test]
    fn classification_preserves_host_availability_and_rejects_self_supersession() {
        let available = dossier();
        let unavailable_result = SourceClassificationOutput {
            sources: vec![SourceClassificationResult {
                source_id: "source-1".to_string(),
                result: ClassificationResultKind::UnavailableOrTruncated,
                requirements: Vec::new(),
                reason: String::new(),
            }],
        };
        assert!(validate_classification(&available, unavailable_result.clone()).is_none());

        let mut unavailable = dossier();
        unavailable.sources[0].availability = UserSourceAvailability::Unavailable;
        assert!(validate_classification(&unavailable, unavailable_result).is_some());
        let non_requirement = SourceClassificationOutput {
            sources: vec![SourceClassificationResult {
                source_id: "source-1".to_string(),
                result: ClassificationResultKind::NonRequirement,
                requirements: Vec::new(),
                reason: "context only".to_string(),
            }],
        };
        assert!(validate_classification(&unavailable, non_requirement).is_none());

        let self_superseded = SourceClassificationOutput {
            sources: vec![SourceClassificationResult {
                source_id: "source-1".to_string(),
                result: ClassificationResultKind::RequirementBearing,
                requirements: vec![ClassificationRequirement {
                    source_span: text_span(0, 15),
                    status: WireRequirementStatus::Superseded,
                    superseded_by_source_id: "source-1".to_string(),
                    superseded_by_span: text_span(0, 15),
                }],
                reason: String::new(),
            }],
        };
        assert!(validate_classification(&available, self_superseded).is_none());
    }

    fn original_finding() -> CompletionReviewFindingReceipt {
        CompletionReviewFindingReceipt {
            finding_id: "review-1/F1".to_string(),
            requirement_ids: vec!["requirement-1".to_string()],
            lens: BEHAVIORAL_LENS.to_string(),
            contract_surface: "bounded owner".to_string(),
            severity: "high".to_string(),
            evidence: "missing behavior".to_string(),
            smallest_correction: "add behavior".to_string(),
            proof_route: "cargo test focused_case".to_string(),
        }
    }

    fn unsatisfied_requirement() -> UnsatisfiedRequirementReviewResult {
        UnsatisfiedRequirementReviewResult {
            requirement_id: "requirement-1".to_string(),
            evidence: "the active requirement remains unsatisfied".to_string(),
        }
    }

    fn disposition(disposition: FindingDisposition) -> ReviewDisposition {
        ReviewDisposition {
            finding_id: "review-1/F1".to_string(),
            disposition,
            evidence: "fresh evidence for the disposition".to_string(),
        }
    }

    #[test]
    fn selector_is_structured_canonical_and_does_not_expand_generic_paths() {
        let generic = ReviewLensSelectionInput {
            task_mutation_paths: vec![ValidatedReviewPath::parse("./src/showcase.rs").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            select_review_lenses(&generic).as_slice(),
            &[BEHAVIORAL_LENS]
        );

        let input = ReviewLensSelectionInput {
            risk_domains: vec![
                ReviewRiskDomain::Security,
                ReviewRiskDomain::Concurrency,
                ReviewRiskDomain::Persistence,
            ],
            task_mutation_paths: vec![ValidatedReviewPath::parse("SRC\\cache.rs").unwrap()],
            surface_roles: vec![ReviewSurfaceRole::Packaging],
            validation_asset_paths: vec![ValidatedReviewPath::parse("tests/golden.snap").unwrap()],
            generated_artifacts: vec![ValidatedReviewPath::parse("generated/output.rs").unwrap()],
            original_finding_lenses: vec![SCHEMA_LENS.to_string(), SECURITY_LENS.to_string()],
            ..Default::default()
        };
        assert_eq!(select_review_lenses(&input).as_slice(), REVIEW_LENSES);
    }

    #[test]
    fn selector_path_validation_is_component_aware_and_generated_artifacts_select_two_lenses() {
        assert!(ValidatedReviewPath::parse("/absolute/cache.rs").is_none());
        assert!(ValidatedReviewPath::parse("C:\\absolute\\cache.rs").is_none());
        assert!(ValidatedReviewPath::parse("../cache.rs").is_none());
        assert!(ValidatedReviewPath::parse("\\\\server\\share\\cache.rs").is_none());

        let cache = ReviewLensSelectionInput {
            task_mutation_paths: vec![ValidatedReviewPath::parse("./src\\cache.rs").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            select_review_lenses(&cache).as_slice(),
            &[BEHAVIORAL_LENS, PIPELINE_LENS]
        );

        let generated = ReviewLensSelectionInput {
            generated_artifacts: vec![ValidatedReviewPath::parse("artifacts/plain.rs").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            select_review_lenses(&generated).as_slice(),
            &[BEHAVIORAL_LENS, SCHEMA_LENS, PIPELINE_LENS]
        );

        let mut malformed = dossier();
        malformed.review_lens_selection_facts.task_mutation_paths =
            vec!["../escape.rs".to_string()];
        assert!(build_review_lens_selection_input(&malformed).is_none());
        malformed
            .review_lens_selection_facts
            .task_mutation_paths
            .clear();
        malformed.review_lens_selection_facts.surface_roles = vec!["invented".to_string()];
        assert!(build_review_lens_selection_input(&malformed).is_none());
    }

    #[test]
    fn selector_maps_every_typed_domain_and_surface_role() {
        let domain_cases = [
            (ReviewRiskDomain::Concurrency, LIFECYCLE_LENS),
            (ReviewRiskDomain::Lifecycle, LIFECYCLE_LENS),
            (ReviewRiskDomain::Persistence, PERSISTENCE_LENS),
            (ReviewRiskDomain::Migration, PERSISTENCE_LENS),
            (ReviewRiskDomain::Rollback, PERSISTENCE_LENS),
            (ReviewRiskDomain::AtomicState, PERSISTENCE_LENS),
            (ReviewRiskDomain::FilesystemSafety, PERSISTENCE_LENS),
            (ReviewRiskDomain::Schema, SCHEMA_LENS),
            (ReviewRiskDomain::Protocol, SCHEMA_LENS),
            (ReviewRiskDomain::Security, SECURITY_LENS),
            (ReviewRiskDomain::Unsafe, SECURITY_LENS),
            (ReviewRiskDomain::Authentication, SECURITY_LENS),
            (ReviewRiskDomain::Permission, SECURITY_LENS),
            (ReviewRiskDomain::Sandbox, SECURITY_LENS),
            (ReviewRiskDomain::TrustBoundary, SECURITY_LENS),
            (ReviewRiskDomain::Installation, PACKAGING_LENS),
            (ReviewRiskDomain::PlatformConfiguration, PACKAGING_LENS),
            (ReviewRiskDomain::Manifest, PACKAGING_LENS),
            (ReviewRiskDomain::Packaging, PACKAGING_LENS),
            (ReviewRiskDomain::Installer, PACKAGING_LENS),
            (ReviewRiskDomain::Publishing, PACKAGING_LENS),
            (ReviewRiskDomain::Release, PACKAGING_LENS),
            (ReviewRiskDomain::Ci, PIPELINE_LENS),
            (ReviewRiskDomain::Cache, PIPELINE_LENS),
            (ReviewRiskDomain::SnapshotProduction, PIPELINE_LENS),
            (ReviewRiskDomain::Generator, PIPELINE_LENS),
            (ReviewRiskDomain::ArtifactIdentity, PIPELINE_LENS),
            (ReviewRiskDomain::Validation, VALIDATION_LENS),
            (ReviewRiskDomain::TestOracle, VALIDATION_LENS),
        ];
        for (domain, expected) in domain_cases {
            let input = ReviewLensSelectionInput {
                risk_domains: vec![domain],
                ..Default::default()
            };
            assert_eq!(
                select_review_lenses(&input).as_slice(),
                &[BEHAVIORAL_LENS, expected]
            );
        }

        let role_cases = [
            (ReviewSurfaceRole::Lifecycle, LIFECYCLE_LENS),
            (ReviewSurfaceRole::Persistence, PERSISTENCE_LENS),
            (ReviewSurfaceRole::Schema, SCHEMA_LENS),
            (ReviewSurfaceRole::Security, SECURITY_LENS),
            (ReviewSurfaceRole::Packaging, PACKAGING_LENS),
            (ReviewSurfaceRole::Pipeline, PIPELINE_LENS),
            (ReviewSurfaceRole::Validation, VALIDATION_LENS),
        ];
        for (role, expected) in role_cases {
            let input = ReviewLensSelectionInput {
                surface_roles: vec![role],
                ..Default::default()
            };
            assert_eq!(
                select_review_lenses(&input).as_slice(),
                &[BEHAVIORAL_LENS, expected]
            );
        }
    }

    #[test]
    fn selector_treats_validation_assets_and_installers_as_exact_structured_signals() {
        let validation_asset = ReviewLensSelectionInput {
            validation_asset_paths: vec![ValidatedReviewPath::parse("quality/plain.data").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            select_review_lenses(&validation_asset).as_slice(),
            &[BEHAVIORAL_LENS, VALIDATION_LENS]
        );

        let installer = ReviewLensSelectionInput {
            task_mutation_paths: vec![ValidatedReviewPath::parse("scripts/install.ps1").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            select_review_lenses(&installer).as_slice(),
            &[BEHAVIORAL_LENS, PACKAGING_LENS]
        );
    }

    #[tokio::test]
    async fn selected_lenses_narrow_dossier_schema_and_prompt_together() {
        let mut dossier = dossier();
        dossier.review_lens_selection_facts.task_mutation_paths = vec!["src/cache.rs".to_string()];
        let selected = selected_lenses(&dossier);
        let expected = json!([BEHAVIORAL_LENS, PIPELINE_LENS]);
        assert_eq!(json!(selected.as_slice()), expected);

        let schema = completion_review_output_schema(&selected);
        assert_eq!(
            schema.pointer("/properties/lens_observations/items/properties/lens/enum"),
            Some(&expected)
        );
        assert_eq!(
            schema.pointer("/properties/findings/items/properties/lens/enum"),
            Some(&expected)
        );
        assert_eq!(
            schema["required"],
            json!([
                "manifest_gaps",
                "unsatisfied_requirements",
                "lens_observations",
                "findings",
                "prior_finding_dispositions"
            ])
        );

        let request_dossier: Value =
            serde_json::from_str(&review_dossier_json(&dossier, false, &selected))
                .expect("review dossier JSON");
        assert_eq!(request_dossier["review_lenses"], expected);

        let inputs = build_reviewer_inputs(
            &dossier,
            ReviewerRequestKind::InitialReview,
            Some(&selected),
        )
        .await
        .expect("review request");
        let UserInput::Text { text, .. } = &inputs[0] else {
            panic!("expected text review request");
        };
        assert!(text.contains("otherwise use requirements_and_behavioral_compatibility"));
        assert!(text.contains("never report a blocking issue only as a lens observation"));
    }

    #[test]
    fn sparse_wire_contract_requires_all_five_arrays_and_rejects_legacy_fields() {
        let complete = json!({
            "manifest_gaps": [],
            "unsatisfied_requirements": [],
            "lens_observations": [],
            "findings": [],
            "prior_finding_dispositions": []
        });
        assert!(serde_json::from_value::<CompletionReviewOutput>(complete.clone()).is_ok());

        for field in [
            "manifest_gaps",
            "unsatisfied_requirements",
            "lens_observations",
            "findings",
            "prior_finding_dispositions",
        ] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<CompletionReviewOutput>(missing).is_err(),
                "missing required array {field} was accepted"
            );
        }

        let mut legacy = complete;
        legacy["clean"] = json!(true);
        assert!(serde_json::from_value::<CompletionReviewOutput>(legacy).is_err());
    }

    #[test]
    fn sparse_review_derives_cleanliness_and_treats_observations_as_advisory() {
        let dossier = dossier();
        let clean = validate(&dossier, clean_output(), false).expect("empty sparse review");
        assert!(clean.review_clean);

        let mut observed = clean_output();
        observed.lens_observations.push(LensObservation {
            lens: BEHAVIORAL_LENS.to_string(),
            surfaces: vec!["coordinator return path".to_string()],
            evidence: "opaque advisory prose may even say blocking without host inference"
                .to_string(),
        });
        let validated = validate(&dossier, observed.clone(), false).expect("advisory observation");
        assert!(validated.review_clean);
        assert_eq!(validated.lens_observations, observed.lens_observations);

        let mut duplicate = observed.clone();
        duplicate
            .lens_observations
            .push(duplicate.lens_observations[0].clone());
        assert!(validate(&dossier, duplicate, false).is_none());
        let mut unselected = observed.clone();
        unselected.lens_observations[0].lens = PIPELINE_LENS.to_string();
        assert!(validate(&dossier, unselected, false).is_none());
        let mut empty_surface = observed;
        empty_surface.lens_observations[0].surfaces.clear();
        assert!(validate(&dossier, empty_surface, false).is_none());

        let mut empty_evidence = clean_output();
        empty_evidence.lens_observations.push(LensObservation {
            lens: BEHAVIORAL_LENS.to_string(),
            surfaces: vec!["coordinator return path".to_string()],
            evidence: "  ".to_string(),
        });
        assert!(validate(&dossier, empty_evidence, false).is_none());
    }

    #[test]
    fn sparse_findings_obey_initial_requirement_set_equation() {
        let dossier = dossier();
        let mut output = clean_output();
        output.findings.push(valid_finding());
        assert!(validate(&dossier, output.clone(), false).is_none());
        output
            .unsatisfied_requirements
            .push(unsatisfied_requirement());
        assert!(
            !validate(&dossier, output.clone(), false)
                .unwrap()
                .review_clean
        );

        let mut second = valid_finding();
        second.finding_local_ordinal = 2;
        second.requirement_ids.clear();
        output.findings.push(second);
        assert!(validate(&dossier, output, false).is_some());

        let mut cross_cutting = clean_output();
        let mut finding = valid_finding();
        finding.requirement_ids.clear();
        cross_cutting.findings.push(finding);
        assert!(
            !validate(&dossier, cross_cutting, false)
                .unwrap()
                .review_clean
        );

        let mut unsupported = clean_output();
        unsupported
            .unsatisfied_requirements
            .push(unsatisfied_requirement());
        assert!(validate(&dossier, unsupported, false).is_none());
    }

    #[test]
    fn sparse_review_rejects_invalid_findings_gaps_and_initial_dispositions() {
        let dossier = dossier();
        let mut gap = clean_output();
        gap.manifest_gaps.push(ManifestGapReviewResult {
            source_id: "source-1".to_string(),
            omitted_source_spans: vec![text_span(15, 24)],
        });
        let validated = validate(&dossier, gap, false).expect("precise manifest gap");
        assert!(!validated.review_clean);
        assert_eq!(validated.manifest_gaps.len(), 1);

        let mut invalid = clean_output();
        let mut finding = valid_finding();
        finding.finding_local_ordinal = 2;
        invalid.findings.push(finding);
        assert!(validate(&dossier, invalid, false).is_none());

        let mut invalid = clean_output();
        let mut finding = valid_finding();
        finding.requirement_ids = vec!["unknown".to_string()];
        invalid.findings.push(finding);
        assert!(validate(&dossier, invalid, false).is_none());

        let mut invalid = clean_output();
        let mut finding = valid_finding();
        finding.concrete_evidence.clear();
        invalid.findings.push(finding);
        assert!(validate(&dossier, invalid, false).is_none());

        let mut invalid = clean_output();
        let mut finding = valid_finding();
        finding.requirement_ids.clear();
        finding.lens = PIPELINE_LENS.to_string();
        invalid.findings.push(finding);
        assert!(validate(&dossier, invalid, false).is_none());

        let mut initial_disposition = clean_output();
        initial_disposition
            .prior_finding_dispositions
            .push(disposition(FindingDisposition::Resolved));
        assert!(validate(&dossier, initial_disposition, false).is_none());
    }

    #[test]
    fn rereview_dispositions_are_exact_and_obey_effective_requirement_set_equation() {
        let mut dossier = dossier();
        dossier.original_findings = vec![original_finding()];

        let mut resolved = clean_output();
        resolved
            .prior_finding_dispositions
            .push(disposition(FindingDisposition::Resolved));
        assert!(
            validate(&dossier, resolved.clone(), true)
                .unwrap()
                .review_clean
        );

        let mut missing = resolved.clone();
        missing.prior_finding_dispositions.clear();
        assert!(validate(&dossier, missing, true).is_none());
        let mut duplicate = resolved.clone();
        duplicate
            .prior_finding_dispositions
            .push(disposition(FindingDisposition::Resolved));
        assert!(validate(&dossier, duplicate, true).is_none());
        let mut unknown = resolved;
        unknown.prior_finding_dispositions[0].finding_id = "review-1/F2".to_string();
        assert!(validate(&dossier, unknown, true).is_none());

        let mut blank_evidence = clean_output();
        let mut blank_disposition = disposition(FindingDisposition::Resolved);
        blank_disposition.evidence = "  ".to_string();
        blank_evidence
            .prior_finding_dispositions
            .push(blank_disposition);
        assert!(validate(&dossier, blank_evidence, true).is_none());

        for unresolved in [
            FindingDisposition::StillPresent,
            FindingDisposition::InsufficientProof,
            FindingDisposition::Regressed,
        ] {
            let mut output = clean_output();
            output
                .prior_finding_dispositions
                .push(disposition(unresolved));
            assert!(validate(&dossier, output.clone(), true).is_none());
            output
                .unsatisfied_requirements
                .push(unsatisfied_requirement());
            assert!(!validate(&dossier, output, true).unwrap().review_clean);
        }
    }

    #[test]
    fn original_finding_identity_binds_every_canonical_field() {
        let finding = original_finding();
        let baseline = original_findings_identity(std::slice::from_ref(&finding)).unwrap();
        for mutation in 0..8 {
            let mut changed = finding.clone();
            match mutation {
                0 => changed.finding_id.push('x'),
                1 => changed.requirement_ids.push("requirement-2".to_string()),
                2 => changed.lens = PIPELINE_LENS.to_string(),
                3 => changed.contract_surface.push('x'),
                4 => changed.severity.push('x'),
                5 => changed.evidence.push('x'),
                6 => changed.smallest_correction.push('x'),
                7 => changed.proof_route.push('x'),
                _ => unreachable!(),
            }
            assert_ne!(
                original_findings_identity(&[changed]).unwrap(),
                baseline,
                "field mutation {mutation} did not change the identity"
            );
        }
    }

    #[test]
    fn observations_flow_only_to_transient_review_advisories() {
        let observations = vec![LensObservation {
            lens: BEHAVIORAL_LENS.to_string(),
            surfaces: vec!["coordinator".to_string()],
            evidence: "context worth surfacing".to_string(),
        }];
        let mut advisories = Vec::new();
        queue_lens_observation_advisories(
            &mut advisories,
            CompletionReviewAttemptKind::InitialReview,
            false,
            "review-1",
            None,
            None,
            &observations,
        );
        assert_eq!(advisories.len(), 1);
        let advisory: Value = serde_json::from_str(&advisories[0]).unwrap();
        assert_eq!(advisory["type"], "completion_review_lens_observation");
        assert_eq!(advisory["lens"], BEHAVIORAL_LENS);

        queue_lens_observation_advisories(
            &mut advisories,
            CompletionReviewAttemptKind::CorrectionEvidence,
            false,
            "review-2",
            None,
            None,
            &observations,
        );
        queue_lens_observation_advisories(
            &mut advisories,
            CompletionReviewAttemptKind::TerminalClosure,
            false,
            "review-3",
            None,
            None,
            &observations,
        );
        assert_eq!(advisories.len(), 1);
    }

    #[test]
    fn evidence_only_correction_requires_an_actionable_local_proof_route() {
        let mut dossier = dossier();
        assert!(build_repair_item(&dossier, &[]).is_none());

        dossier.locally_obtainable_proof_routes =
            vec!["run the focused generated-artifact proof and record its receipt".to_string()];
        let (_, payload) = build_repair_item(&dossier, &[]).expect("actionable correction");
        let payload: Value = serde_json::from_str(&payload).expect("correction JSON");
        assert_eq!(
            payload["applicable_proof_routes"],
            json!(["run the focused generated-artifact proof and record its receipt"])
        );
        assert_eq!(payload["complete_finding_set"], json!([]));
    }

    #[test]
    fn reviewer_requests_only_expose_dossier_bound_evidence() {
        let mut dossier = dossier();
        dossier.locally_obtainable_proof_routes = vec!["run focused proof".to_string()];

        let selected = selected_lenses(&dossier);
        let review: Value = serde_json::from_str(&review_dossier_json(&dossier, false, &selected))
            .expect("review JSON");
        assert!(review.get("evidence_summary").is_none());
        assert_eq!(
            review["reviewer_visible_evidence"],
            dossier.reviewer_visible_evidence
        );

        let (_, correction) = build_repair_item(&dossier, &[]).expect("correction payload");
        let correction: Value = serde_json::from_str(&correction).expect("correction JSON");
        assert!(correction.get("evidence_summary").is_none());
        assert_eq!(
            correction["reviewer_visible_evidence"],
            dossier.reviewer_visible_evidence
        );
    }

    #[test]
    fn host_mints_canonical_finding_ids_from_local_ordinals() {
        let findings = vec![CompletionReviewFindingInput {
            local_ordinal: 1,
            requirement_ids: vec!["requirement-1".to_string()],
            lens: REVIEW_LENSES[0].to_string(),
            contract_surface: "bounded owner".to_string(),
            severity: "high".to_string(),
            evidence: "missing behavior".to_string(),
            smallest_correction: "add behavior".to_string(),
            proof_route: "cargo test focused_case".to_string(),
        }];
        let receipts = preview_finding_receipts("review-7", &findings);
        assert_eq!(receipts[0].finding_id, "review-7/F1");
    }

    #[tokio::test]
    async fn image_bytes_are_attached_once_and_never_embedded_in_text_dossiers() {
        let mut dossier = dossier();
        let image_payload = format!("DaTa:image/png;base64,{}", "A".repeat(50_000));
        let image_hash = format!("{:x}", Sha256::digest(image_payload.as_bytes()));
        dossier.sources[0].source_kind = UserSourceKind::Image;
        dossier.sources[0].content_hash = image_hash.clone();
        dossier.sources[0].exact_material = image_payload.clone();
        dossier.requirements[0].source_content_hash = image_hash;
        dossier.requirements[0].source_span = SourceSpan::Image {
            reference: image_payload.clone(),
            region: None,
        };
        dossier.requirements[0].exact_material = image_payload.clone();

        let reviewer_reference = reviewer_source_reference(&dossier.sources[0]);
        let classification_json = classification_dossier_json(&dossier);
        let selected = selected_lenses(&dossier);
        let review_json = review_dossier_json(&dossier, false, &selected);
        assert!(!classification_json.contains(&image_payload));
        assert!(!review_json.contains(&image_payload));
        assert!(classification_json.contains(&reviewer_reference));
        assert!(review_json.contains(&reviewer_reference));

        for kind in [
            ReviewerRequestKind::Classification,
            ReviewerRequestKind::InitialReview,
        ] {
            let selected_arg =
                matches!(&kind, ReviewerRequestKind::InitialReview).then_some(&selected);
            let inputs = build_reviewer_inputs(&dossier, kind, selected_arg)
                .await
                .expect("bounded reviewer inputs");
            assert_eq!(inputs.len(), 2);
            match &inputs[0] {
                UserInput::Text { text, .. } => {
                    assert!(!text.contains(&image_payload));
                    assert!(text.contains(&reviewer_reference));
                }
                other => panic!("expected text dossier, got {other:?}"),
            }
            match &inputs[1] {
                UserInput::Image { image_url, .. } => assert_eq!(image_url, &image_payload),
                other => panic!("expected one image attachment, got {other:?}"),
            }
        }

        let classification = SourceClassificationOutput {
            sources: vec![SourceClassificationResult {
                source_id: dossier.sources[0].source_id.clone(),
                result: ClassificationResultKind::RequirementBearing,
                requirements: vec![ClassificationRequirement {
                    source_span: WireSpan {
                        kind: "image".to_string(),
                        start: 0,
                        end: 0,
                        reference: reviewer_reference.clone(),
                        subreference: String::new(),
                    },
                    status: WireRequirementStatus::Active,
                    superseded_by_source_id: String::new(),
                    superseded_by_span: empty_span(),
                }],
                reason: String::new(),
            }],
        };
        let classified = validate_classification(&dossier, classification)
            .expect("bounded reference maps to immutable source material");
        assert_eq!(
            classified[0].requirements[0].source_span,
            SourceSpan::Image {
                reference: image_payload.clone(),
                region: None,
            }
        );

        let finding = CompletionReviewFindingReceipt {
            finding_id: "review-1/F1".to_string(),
            requirement_ids: vec!["requirement-1".to_string()],
            lens: REVIEW_LENSES[0].to_string(),
            contract_surface: "bounded owner".to_string(),
            severity: "high".to_string(),
            evidence: "missing behavior".to_string(),
            smallest_correction: "add behavior".to_string(),
            proof_route: "cargo test focused_case".to_string(),
        };
        let (_, repair_payload) =
            build_repair_item(&dossier, &[finding]).expect("bounded repair payload");
        assert!(!repair_payload.contains(&image_payload));
        assert!(repair_payload.contains(&reviewer_reference));
    }

    #[tokio::test]
    async fn reviewer_images_require_complete_bounded_coverage() {
        let mut review_dossier = dossier();
        review_dossier.sources = (1..=MAX_RETAINED_USER_IMAGES)
            .map(|ordinal| {
                let mut source = review_dossier.sources[0].clone();
                source.source_id = format!("source-{ordinal}");
                source.source_ordinal = ordinal as u64;
                source.source_kind = UserSourceKind::Image;
                source.exact_material = format!("data:image/png;base64,{ordinal}");
                source
            })
            .collect();

        let inputs =
            build_reviewer_inputs(&review_dossier, ReviewerRequestKind::Classification, None)
                .await
                .expect("the exact image-count limit should fit");
        assert_eq!(inputs.len(), MAX_RETAINED_USER_IMAGES + 1);

        let mut extra = review_dossier.sources[0].clone();
        extra.source_id = "source-over-limit".to_string();
        review_dossier.sources.push(extra);
        assert!(matches!(
            build_reviewer_inputs(&review_dossier, ReviewerRequestKind::Classification, None,)
                .await,
            Err(ReviewFailureCategory::OversizedRequest)
        ));
    }

    #[tokio::test]
    async fn reviewer_local_images_use_file_size_for_the_aggregate_byte_bound() {
        let temp = tempfile::tempdir().expect("tempdir");
        let image_bytes = (MAX_RETAINED_USER_IMAGE_BYTES / 2 + 1) as u64;
        let mut review_dossier = dossier();
        review_dossier.sources.clear();
        for ordinal in 1..=2 {
            let path = temp.path().join(format!("image-{ordinal}.png"));
            let file = tokio::fs::File::create(&path).await.expect("image fixture");
            file.set_len(image_bytes)
                .await
                .expect("set logical image size");
            let mut source = dossier().sources.remove(0);
            source.source_id = format!("image-source-{ordinal}");
            source.source_ordinal = ordinal;
            source.source_kind = UserSourceKind::Image;
            source.exact_material = format!(
                "local-image:{}#sha256={}",
                path.to_string_lossy(),
                "a".repeat(64)
            );
            review_dossier.sources.push(source);
        }

        assert!(matches!(
            build_reviewer_inputs(&review_dossier, ReviewerRequestKind::Classification, None,)
                .await,
            Err(ReviewFailureCategory::OversizedRequest)
        ));
    }

    #[test]
    fn saturated_authoritative_mutation_page_is_partial() {
        assert!(authoritative_mutation_page_saturation_reason("assignment-1", 99).is_none());
        let reason = authoritative_mutation_page_saturation_reason(
            "assignment-1",
            AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT,
        )
        .expect("a maximum-sized page must be treated as incomplete");
        assert!(reason.contains("additional mutation evidence may be omitted"));
    }

    #[tokio::test]
    async fn file_backed_sources_are_rehashed_for_review_and_terminal_freshness() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("immutable-source.bin");
        let original = (0..1024 * 1024 + 17)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        tokio::fs::write(&path, &original)
            .await
            .expect("write source fixture");
        let path = path.to_string_lossy().into_owned();
        let hash = format!("{:x}", Sha256::digest(&original));

        for (kind, material) in [
            (
                UserSourceKind::Image,
                format!("local-image:{path}#sha256={hash}"),
            ),
            (
                UserSourceKind::Attachment,
                format!("skill:fixture-skill:{path}#sha256={hash}"),
            ),
        ] {
            tokio::fs::write(&path, &original)
                .await
                .expect("restore source fixture");
            let mut dossier = dossier();
            dossier.sources[0].source_kind = kind;
            dossier.sources[0].exact_material = material;
            assert!(user_sources_still_current(&dossier).await);

            tokio::fs::write(&path, b"changed source bytes")
                .await
                .expect("mutate source fixture");
            assert!(!user_sources_still_current(&dossier).await);

            tokio::fs::remove_file(&path)
                .await
                .expect("remove source fixture");
            assert!(!user_sources_still_current(&dossier).await);
        }
    }
}
