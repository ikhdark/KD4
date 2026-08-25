use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationProofKind;
use codex_features::Feature;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::approx_token_count;
use futures::StreamExt;
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
use crate::codex_delegate::PreparedCodexOneShot;
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
use crate::task_evidence::CompletionReviewAuditMeasurements;
use crate::task_evidence::CompletionReviewCyclePhase;
use crate::task_evidence::CompletionReviewDispositionReceipt;
use crate::task_evidence::CompletionReviewDossier;
use crate::task_evidence::CompletionReviewFindingInput;
use crate::task_evidence::CompletionReviewFindingReceipt;
use crate::task_evidence::CompletionReviewObligationInput;
use crate::task_evidence::LocalSemanticCue;
use crate::task_evidence::LocalSemanticCueKind;
use crate::task_evidence::ManifestGapInput;
use crate::task_evidence::PriorCompletionReviewAttempt;
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
use crate::task_evidence::TypedValidationProofInputV1;
use crate::task_evidence::UserSourceAvailability;
use crate::task_evidence::UserSourceKind;
use crate::task_evidence::UserSourceRecord;
use crate::task_evidence::build_repair_baseline;
use crate::task_evidence::repair_baseline_hash;
use crate::task_evidence::sha256_file;
use crate::task_evidence::source_classification_cache_key;
use crate::task_evidence::source_local_classification_is_valid_for_source;
use crate::task_evidence::source_local_classifications_with_manifest_gaps;
use crate::turn_diff_tracker::ValidationFreshnessStatus;

const REVIEW_DEADLINE: Duration = Duration::from_secs(90);
const REVIEW_CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
const MAX_RENDERED_REQUEST_TOKENS: usize = 8_999;
const MAX_REVIEW_OUTPUT_TOKENS: usize = 6_000;
const MAX_REVIEW_FINDINGS: usize = 32;
const MAX_REVIEW_REQUIREMENTS: usize = 256;
const AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT: usize = 100;
const AUTHORITATIVE_BINDING_READ_CONCURRENCY: usize = 8;

const SOURCE_CLASSIFICATION_MARKER: &str = "KD4_SOURCE_CLASSIFICATION_REQUEST_V1";
const SOURCE_LOCAL_CLASSIFICATION_MARKER: &str = "KD4_SOURCE_LOCAL_CLASSIFICATION_REQUEST_V4";
const SOURCE_RELATIONSHIP_RESOLUTION_MARKER: &str = "KD4_SOURCE_RELATIONSHIP_RESOLUTION_REQUEST_V1";
const REVIEW_REQUEST_MARKER: &str = "KD4_COMPLETION_REVIEW_REQUEST_V2";
const REVIEWER_EXECUTION_CONTRACT_VERSION: &str = "kd4-reviewer-execution-v1";
const REVIEW_DOSSIER_CONTRACT_VERSION: &str = "kd4-bounded-dossier-v1";
const REVIEW_OUTPUT_SCHEMA_VERSION: &str = "kd4-review-output-v2";

const BEHAVIORAL_LENS: &str = "requirements_and_behavioral_compatibility";
const LIFECYCLE_LENS: &str = "lifecycle_and_concurrency";
const PERSISTENCE_LENS: &str = "persistence_filesystem_safety_rollback_and_atomicity";
const SCHEMA_LENS: &str = "schema_protocol_and_generated_representations";
const SECURITY_LENS: &str = "security_and_trust_boundaries";
const PACKAGING_LENS: &str = "platform_configuration_packaging_and_installation";
const PIPELINE_LENS: &str = "pipeline_cache_snapshot_and_artifact_identity";
const VALIDATION_LENS: &str = "validation_quality_and_changed_test_oracle_integrity";

#[path = "completion_review_lenses.rs"]
mod completion_review_lenses;

#[cfg(test)]
use completion_review_lenses::ReviewLensSelectionInput;
#[cfg(test)]
use completion_review_lenses::ReviewRiskDomain;
#[cfg(test)]
use completion_review_lenses::ReviewSurfaceRole;
use completion_review_lenses::SelectedReviewLenses;
#[cfg(test)]
use completion_review_lenses::ValidatedReviewPath;
use completion_review_lenses::build_review_lens_selection_input;
use completion_review_lenses::select_review_lenses;

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
    ready_phase_recorded: bool,
    terminal_phase_recorded: bool,
}

impl CompletionReviewState {
    fn record_current_phase(&mut self, timing: &crate::turn_timing::TurnTimingState) {
        match self.phase {
            TurnReviewPhase::Ready if !self.ready_phase_recorded => {
                self.ready_phase_recorded = true;
                timing.record_completion_review_ready_phase();
            }
            TurnReviewPhase::Terminal if !self.terminal_phase_recorded => {
                self.terminal_phase_recorded = true;
                timing.record_completion_review_terminal_phase();
            }
            TurnReviewPhase::Ready
            | TurnReviewPhase::CorrectionInjected
            | TurnReviewPhase::Terminal => {}
        }
    }
}

#[derive(Clone, Copy)]
enum ReviewTelemetryPhase {
    Preflight,
    Review,
}

struct ReviewTelemetryTimer {
    timing: Arc<crate::turn_timing::TurnTimingState>,
    phase: ReviewTelemetryPhase,
    started: Instant,
    active: bool,
}

impl ReviewTelemetryTimer {
    fn start(
        timing: Arc<crate::turn_timing::TurnTimingState>,
        phase: ReviewTelemetryPhase,
    ) -> Self {
        Self {
            timing,
            phase,
            started: Instant::now(),
            active: true,
        }
    }

    fn finish(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let elapsed = self.started.elapsed();
        match self.phase {
            ReviewTelemetryPhase::Preflight => {
                self.timing.record_completion_review_preflight(elapsed);
            }
            ReviewTelemetryPhase::Review => self.timing.record_completion_review(elapsed),
        }
    }
}

impl Drop for ReviewTelemetryTimer {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
enum ReviewObligationMode {
    Mandatory {
        requirement_ids: Vec<String>,
        obligation_hash: String,
    },
    Supplemental,
    Disabled,
}

impl ReviewObligationMode {
    fn name(&self) -> &'static str {
        match self {
            Self::Mandatory { .. } => "mandatory",
            Self::Supplemental => "supplemental",
            Self::Disabled => "disabled",
        }
    }

    fn hash(&self) -> String {
        match self {
            Self::Mandatory {
                obligation_hash, ..
            } => obligation_hash.clone(),
            Self::Supplemental => stable_hash(&json!({ "mode": "supplemental" })),
            Self::Disabled => stable_hash(&json!({ "mode": "disabled" })),
        }
    }

    fn requirement_ids(&self) -> Vec<String> {
        match self {
            Self::Mandatory {
                requirement_ids, ..
            } => requirement_ids.clone(),
            Self::Supplemental | Self::Disabled => Vec::new(),
        }
    }

    fn is_mandatory(&self) -> bool {
        matches!(self, Self::Mandatory { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReviewObligationResolution {
    Resolved(ReviewObligationMode),
    NeedsObligationMaterialization,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewAdmissionDecision {
    Admit,
    NotAdmittedCorrectness,
    SkipNonMutating,
    SkipDocumentationOnly,
    SkipFreshLowRisk,
    RejectSelfReview,
}

impl ReviewAdmissionDecision {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::NotAdmittedCorrectness => "not_admitted_correctness",
            Self::SkipNonMutating => "skip_non_mutating",
            Self::SkipDocumentationOnly => "skip_documentation_only",
            Self::SkipFreshLowRisk => "skip_fresh_low_risk",
            Self::RejectSelfReview => "reject_self_review",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompletionReviewTurnEvidence {
    pub(crate) exact_diff: Option<String>,
    pub(crate) mutation_revision: u64,
    pub(crate) validation_freshness: ValidationFreshnessStatus,
    pub(crate) last_successful_validation_revision: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ReviewerExecutionContract {
    contract_version: &'static str,
    reviewer_model: String,
    reviewer_provider: String,
    reasoning_configuration: String,
    reviewer_prompt_hash: String,
    output_schema_version: &'static str,
    tool_capability_hash: String,
    source_classification_contract_version: &'static str,
    relationship_resolver_contract_version: &'static str,
    review_feature_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReviewAttemptIdentity {
    value: String,
    reviewer_contract_hash: String,
    bounded_dossier_hash: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewFailureClass {
    Deterministic,
    Availability,
    ExecutionBounded,
}

impl ReviewFailureClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Availability => "availability",
            Self::ExecutionBounded => "execution_bounded",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct CompletionReviewCoordinatorOutcome {
    pub(crate) repair_injected: bool,
    pub(crate) provisional_clean: bool,
    pub(crate) advisory: Option<String>,
    pub(crate) partial_reasons: Vec<String>,
    candidate_changed: bool,
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
    InvalidDossier,
    UnsupportedConfiguration,
    SelfReviewProhibited,
}

impl ReviewFailureCategory {
    const fn is_review_infrastructure(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Capacity
                | Self::SpawnModel
                | Self::MalformedOutput
                | Self::OversizedOutput
                | Self::OversizedRequest
                | Self::Cleanup
        )
    }

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
            Self::InvalidDossier => "invalid_or_incomplete_dossier",
            Self::UnsupportedConfiguration => "unsupported_reviewer_configuration",
            Self::SelfReviewProhibited => "self_review_prohibited",
        }
    }

    const fn class(self) -> ReviewFailureClass {
        match self {
            Self::Capacity | Self::SpawnModel => ReviewFailureClass::Availability,
            Self::Timeout | Self::MalformedOutput | Self::OversizedOutput | Self::Cleanup => {
                ReviewFailureClass::ExecutionBounded
            }
            Self::OversizedRequest
            | Self::Persistence
            | Self::InputUnavailable
            | Self::SourceDrift
            | Self::RepeatedManifestGap
            | Self::InvalidDossier
            | Self::UnsupportedConfiguration
            | Self::SelfReviewProhibited => ReviewFailureClass::Deterministic,
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
            Self::InvalidDossier => "completion review dossier was incomplete or invalid",
            Self::UnsupportedConfiguration => "completion reviewer configuration is unsupported",
            Self::SelfReviewProhibited => "completion reviewer children cannot review themselves",
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
    elapsed_millis: u64,
    logical_generations: u64,
    physical_requests: u64,
    tool_calls: u64,
}

impl ReviewerExecution {
    fn failed(category: ReviewFailureCategory) -> Self {
        Self {
            payload: None,
            failures: vec![category],
            elapsed_millis: 0,
            logical_generations: 0,
            physical_requests: 0,
            tool_calls: 0,
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
                                            "relationship_only_context",
                                            "mandatory_completion_review",
                                            "supplemental_completion_review"
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

const REVIEWER_DISABLED_FEATURES: &[Feature] = &[
    // A completion reviewer must not run completion review on its own turn.
    // Otherwise, enabling the reviewer for the parent creates an unbounded
    // chain of reviewer threads and the parent turn never terminates.
    Feature::TaskCompletionReviewer,
    Feature::SpawnCsv,
    Feature::Collab,
    Feature::MultiAgentV2,
    Feature::Apps,
    Feature::Plugins,
    Feature::WebSearchRequest,
    Feature::WebSearchCached,
    Feature::CodeMode,
    Feature::CodeModeHost,
    Feature::CodeModeOnly,
    Feature::CodexHooks,
    Feature::Personality,
];

fn disable_reviewer_features(config: &mut Config) -> Result<(), ()> {
    for &feature in REVIEWER_DISABLED_FEATURES {
        config.features.disable(feature).map_err(|_| ())?;
        if config.features.enabled(feature) {
            return Err(());
        }
    }
    Ok(())
}

async fn build_reviewer_config(
    turn_context: &TurnContext,
    requires_images: bool,
) -> Result<Config, ()> {
    let mut config = turn_context.config.as_ref().clone();
    if !config.agent_roles.contains_key("reviewer") {
        return Err(());
    }
    let inherited_model_provider = config.model_provider.clone();
    apply_role_to_config(&mut config, Some("reviewer"))
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
    disable_reviewer_features(&mut config)?;
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
    let mut reviewer_turn_id = None;
    let session_loop_termination = io.session_loop_termination.clone();
    let (raw_output, timing) = loop {
        let event = match io.next_event().await {
            Ok(event) => event,
            Err(_) => {
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
                break (completed.last_agent_message, completed.timing);
            }
            EventMsg::TurnAborted(aborted)
                if reviewer_turn_id.as_deref() == aborted.turn_id.as_deref() =>
            {
                return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel);
            }
            _ => {}
        }
    };
    if timeout(REVIEW_CLEANUP_DEADLINE, session_loop_termination)
        .await
        .is_err()
    {
        return ReviewerExecution::failed(ReviewFailureCategory::Cleanup);
    }
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
    let (elapsed_millis, logical_generations, physical_requests, tool_calls) = timing
        .map(|timing| {
            (
                timing.inclusive_duration_ms,
                u64::from(timing.counters.logical_generation_count),
                u64::from(timing.counters.model_request_count),
                u64::from(timing.counters.tool_call_count),
            )
        })
        .unwrap_or_default();
    match payload {
        Some(payload) => ReviewerExecution {
            payload: Some(payload),
            failures: Vec::new(),
            elapsed_millis,
            logical_generations,
            physical_requests,
            tool_calls,
        },
        None => ReviewerExecution::failed(ReviewFailureCategory::MalformedOutput),
    }
}

async fn run_prepared_reviewer_with_deadline(
    prepared: PreparedCodexOneShot,
    inputs: Vec<UserInput>,
    schema: Value,
    kind: ReviewerRequestKind,
    review_cancellation: CancellationToken,
    parent_cancellation: &CancellationToken,
) -> CodexResult<ReviewerExecution> {
    let mut run = Box::pin(async move {
        match prepared.submit_once(inputs, Some(schema)).await {
            Ok(io) => collect_reviewer_execution(io, kind).await,
            Err(_) => ReviewerExecution::failed(ReviewFailureCategory::SpawnModel),
        }
    });
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

async fn collect_reviewer_execution(
    io: crate::session::Codex,
    kind: ReviewerRequestKind,
) -> ReviewerExecution {
    let mut reviewer_turn_id = None;
    let session_loop_termination = io.session_loop_termination.clone();
    let (raw_output, timing) = loop {
        let event = match io.next_event().await {
            Ok(event) => event,
            Err(_) => {
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
                break (completed.last_agent_message, completed.timing);
            }
            EventMsg::TurnAborted(aborted)
                if reviewer_turn_id.as_deref() == aborted.turn_id.as_deref() =>
            {
                return ReviewerExecution::failed(ReviewFailureCategory::SpawnModel);
            }
            _ => {}
        }
    };
    if timeout(REVIEW_CLEANUP_DEADLINE, session_loop_termination)
        .await
        .is_err()
    {
        return ReviewerExecution::failed(ReviewFailureCategory::Cleanup);
    }
    let Some(raw_output) = raw_output else {
        return ReviewerExecution::failed(ReviewFailureCategory::MalformedOutput);
    };
    if approx_token_count(&raw_output) > MAX_REVIEW_OUTPUT_TOKENS {
        return ReviewerExecution::failed(ReviewFailureCategory::OversizedOutput);
    }
    let payload = match kind {
        ReviewerRequestKind::InitialReview | ReviewerRequestKind::Rereview => {
            serde_json::from_str(&raw_output)
                .ok()
                .map(ReviewerPayload::Review)
        }
        _ => None,
    };
    let (elapsed_millis, logical_generations, physical_requests, tool_calls) = timing
        .map(|timing| {
            (
                timing.inclusive_duration_ms,
                u64::from(timing.counters.logical_generation_count),
                u64::from(timing.counters.model_request_count),
                u64::from(timing.counters.tool_call_count),
            )
        })
        .unwrap_or_default();
    match payload {
        Some(payload) => ReviewerExecution {
            payload: Some(payload),
            failures: Vec::new(),
            elapsed_millis,
            logical_generations,
            physical_requests,
            tool_calls,
        },
        None => ReviewerExecution {
            payload: None,
            failures: vec![ReviewFailureCategory::MalformedOutput],
            elapsed_millis,
            logical_generations,
            physical_requests,
            tool_calls,
        },
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

    inputs_with_source_images(dossier, request).await
}

async fn build_final_reviewer_inputs(
    dossier: &CompletionReviewDossier,
    kind: ReviewerRequestKind,
    selected_lenses: &SelectedReviewLenses,
    obligation: &ReviewObligationMode,
    turn_evidence: &CompletionReviewTurnEvidence,
) -> Result<(Vec<UserInput>, String), ReviewFailureCategory> {
    let rereview = matches!(kind, ReviewerRequestKind::Rereview);
    if !matches!(
        kind,
        ReviewerRequestKind::InitialReview | ReviewerRequestKind::Rereview
    ) {
        return Err(ReviewFailureCategory::InvalidDossier);
    }
    let bounded_dossier = bounded_review_dossier_json(
        dossier,
        rereview,
        selected_lenses,
        obligation,
        turn_evidence,
    )?;
    let instructions = if rereview {
        "attempt_kind=rereview\nIndependently rereview the frozen original findings and the correction delta. Return all five required arrays. Disposition every original finding exactly once. Report only actionable requirement or contract failures; observations remain advisory."
    } else {
        "Independently review this exact candidate. Return all five required arrays. Report only actionable active-requirement failures, affected contract incompatibilities, or missing required proof. Observations remain advisory."
    };
    let request = format!(
        "{REVIEW_REQUEST_MARKER}\n\n{instructions}\nThe host validates identity, completeness, contradictions, freshness, and cleanliness.\n\n<completion_dossier>\n{bounded_dossier}\n</completion_dossier>"
    );
    if approx_token_count(&request) > MAX_RENDERED_REQUEST_TOKENS {
        return Err(ReviewFailureCategory::OversizedRequest);
    }
    let inputs = inputs_with_source_images(dossier, request).await?;
    Ok((inputs, bounded_dossier))
}

async fn inputs_with_source_images(
    dossier: &CompletionReviewDossier,
    request: String,
) -> Result<Vec<UserInput>, ReviewFailureCategory> {
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
    bounded_review_dossier_json(
        dossier,
        rereview,
        selected_lenses,
        &ReviewObligationMode::Supplemental,
        &CompletionReviewTurnEvidence {
            exact_diff: None,
            mutation_revision: dossier.host_mutation_revision,
            validation_freshness: ValidationFreshnessStatus::None,
            last_successful_validation_revision: None,
        },
    )
    .unwrap_or_else(|_| "{}".to_string())
}

fn bounded_review_dossier_json(
    dossier: &CompletionReviewDossier,
    rereview: bool,
    selected_lenses: &SelectedReviewLenses,
    obligation: &ReviewObligationMode,
    turn_evidence: &CompletionReviewTurnEvidence,
) -> Result<String, ReviewFailureCategory> {
    let sources = reviewer_visible_sources(dossier);
    let requirements = reviewer_visible_requirements(dossier)
        .into_iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .map(|requirement| {
            json!({
                "requirement_id": requirement.requirement_id,
                "source_id": requirement.source_id,
                "source_span": requirement.source_span,
            })
        })
        .collect::<Vec<_>>();
    let unique_requirement_ids = requirements
        .iter()
        .filter_map(|requirement| requirement.get("requirement_id")?.as_str())
        .collect::<BTreeSet<_>>();
    if requirements.len() > MAX_REVIEW_REQUIREMENTS
        || unique_requirement_ids.len() != requirements.len()
    {
        return Err(ReviewFailureCategory::InvalidDossier);
    }
    let validation_summary = json!({
        "ordinary_gate": dossier.evidence_gate,
        "freshness": format!("{:?}", turn_evidence.validation_freshness),
        "mutation_revision": turn_evidence.mutation_revision,
        "last_successful_validation_revision": turn_evidence.last_successful_validation_revision,
        "focused_receipts": dossier.reviewer_visible_evidence.get("proofReceipts"),
        "external_evidence": dossier.reviewer_visible_evidence.get("externalEvidence"),
    });
    let task_attributed_paths = dossier
        .reviewer_visible_evidence
        .get("taskAttributedPaths")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let serialized = serde_json::to_string_pretty(&json!({
        "dossier_contract": REVIEW_DOSSIER_CONTRACT_VERSION,
        "root_task_id": dossier.root_task_id,
        "completion_epoch": dossier.completion_epoch,
        "manifest_revision": dossier.manifest_revision,
        "user_source_ledger_hash": dossier.user_source_ledger_hash,
        "source_capture_failed": dossier.source_capture_failed,
        "requirement_manifest_hash": dossier.requirement_manifest_hash,
        "implementation_identity": dossier.implementation_identity_hash,
        "dossier_snapshot_id": dossier.dossier_snapshot_id,
        "obligation": obligation,
        "sources": sources,
        "requirements": requirements,
        "task_attributed_paths": task_attributed_paths,
        "exact_current_diff": turn_evidence.exact_diff,
        "validation": validation_summary,
        "authoritative_input_errors": dossier.authoritative_input_errors,
        "typed_quiescent": dossier.typed_quiescent,
        "default_children_quiescent": dossier.default_children_quiescent,
        "structured_risks": dossier.review_lens_selection_facts,
        "candidate_completion": dossier.candidate_completion,
        "review_lenses": selected_lenses.as_slice(),
        "rereview": rereview,
        "cycle_parent_review_id": dossier.cycle_parent_review_id,
        "cycle_superseded_review_id": dossier.cycle_superseded_review_id,
        "initial_review_id": dossier.initial_review_id,
        "original_findings": dossier.original_findings,
        "repair_lineage": dossier.rereview_input,
    }))
    .map_err(|_| ReviewFailureCategory::InvalidDossier)?;
    Ok(serialized)
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
        "{SOURCE_LOCAL_CLASSIFICATION_MARKER}\n\nClassify every supplied cache-miss item exactly once and in the supplied order. Each item is one immutable source-local classification key, not one relationship occurrence. Inspect only that item's exact material. Return exact requirement spans and source-local semantic cues. Mark mandatory_completion_review only when a requirement explicitly requires completion review; mark supplemental_completion_review only when it explicitly makes completion review optional or supplemental. Never infer either cue from feature enablement, general quality language, risk, or review-related discussion. Bind each such cue to the exact requirement span. Do not assign active, superseded, or withdrawn status; do not compare sources; do not author cross-source relationships. Text spans are UTF-8 byte offsets; image and attachment spans use the supplied immutable reference. reason must be nonempty.\n\n<source_local_items>\n{}\n</source_local_items>",
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
        if local_semantic_cues.iter().any(|cue| {
            matches!(
                cue.kind,
                LocalSemanticCueKind::MandatoryCompletionReview
                    | LocalSemanticCueKind::SupplementalCompletionReview
            ) && cue
                .source_span
                .as_ref()
                .is_none_or(|span| !requirement_spans.contains(span))
        }) {
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

    let materialization = source_materialization_from_resolved(dossier, resolved_sources)
        .ok_or(ReviewFailureCategory::MalformedOutput);
    Ok(materialization)
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

fn stable_hash(value: &impl Serialize) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", Sha256::digest(encoded))
}

fn resolve_review_obligation(dossier: &CompletionReviewDossier) -> ReviewObligationResolution {
    let active = dossier
        .requirements
        .iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return ReviewObligationResolution::Resolved(ReviewObligationMode::Supplemental);
    }
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut mandatory_ids = Vec::new();
    for requirement in active {
        let Some(source) = sources.get(requirement.source_id.as_str()) else {
            return ReviewObligationResolution::NeedsObligationMaterialization;
        };
        let key = source_classification_cache_key(source);
        let Some(classification) = dossier.source_classification_cache.get(&key) else {
            return ReviewObligationResolution::NeedsObligationMaterialization;
        };
        for cue in &classification.local_semantic_cues {
            if cue.source_span.as_ref() != Some(&requirement.source_span) {
                continue;
            }
            match cue.kind {
                LocalSemanticCueKind::MandatoryCompletionReview => {
                    mandatory_ids.push(requirement.requirement_id.clone());
                }
                LocalSemanticCueKind::SupplementalCompletionReview => {}
                LocalSemanticCueKind::Assertion
                | LocalSemanticCueKind::ReplacementIntent
                | LocalSemanticCueKind::WithdrawalIntent
                | LocalSemanticCueKind::RelationshipOnlyContext => {}
            }
        }
    }
    mandatory_ids.sort();
    mandatory_ids.dedup();
    if mandatory_ids.is_empty() {
        return ReviewObligationResolution::Resolved(ReviewObligationMode::Supplemental);
    }
    let obligation_hash = stable_hash(&json!({
        "mode": "mandatory",
        "requirement_ids": mandatory_ids,
    }));
    ReviewObligationResolution::Resolved(ReviewObligationMode::Mandatory {
        requirement_ids: mandatory_ids,
        obligation_hash,
    })
}

fn resolve_disabled_review_requirement(dossier: &CompletionReviewDossier) -> ReviewObligationMode {
    match resolve_review_obligation(dossier) {
        ReviewObligationResolution::Resolved(obligation) if obligation.is_mandatory() => obligation,
        ReviewObligationResolution::Resolved(_)
        | ReviewObligationResolution::NeedsObligationMaterialization => {
            ReviewObligationMode::Disabled
        }
    }
}

fn is_documentation_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let extension = Path::new(&normalized)
        .extension()
        .and_then(|extension| extension.to_str());
    normalized.starts_with("docs/")
        || normalized.contains("/docs/")
        || matches!(extension, Some("md" | "mdx" | "rst" | "adoc" | "txt"))
}

fn review_admission_decision(
    turn_context: &TurnContext,
    dossier: &CompletionReviewDossier,
    obligation: &ReviewObligationMode,
    turn_evidence: &CompletionReviewTurnEvidence,
) -> ReviewAdmissionDecision {
    review_admission_decision_for_source(
        &turn_context.session_source,
        dossier,
        obligation,
        turn_evidence,
    )
}

fn review_admission_decision_for_source(
    session_source: &SessionSource,
    dossier: &CompletionReviewDossier,
    obligation: &ReviewObligationMode,
    turn_evidence: &CompletionReviewTurnEvidence,
) -> ReviewAdmissionDecision {
    if matches!(
        session_source,
        SessionSource::SubAgent(SubAgentSource::Review)
    ) {
        return ReviewAdmissionDecision::RejectSelfReview;
    }
    if obligation.is_mandatory() {
        return ReviewAdmissionDecision::Admit;
    }
    let has_turn_diff = turn_evidence
        .exact_diff
        .as_deref()
        .is_some_and(|diff| !diff.trim().is_empty());
    let has_mutations = dossier.has_task_attributed_mutations || has_turn_diff;
    let fresh_validation = turn_evidence.validation_freshness
        == ValidationFreshnessStatus::PassedAfterLastMutation
        && turn_evidence.last_successful_validation_revision
            == Some(turn_evidence.mutation_revision);
    let deterministic_completion_evidence_sufficient = dossier.mappings_classified
        && dossier.source_classification_current
        && dossier.relationship_resolution_current
        && !dossier.source_capture_failed
        && dossier
            .sources
            .iter()
            .all(|source| source.availability == UserSourceAvailability::Available)
        && dossier.evidence_gate.status == TaskCompletionStatus::Passed
        && dossier.authoritative_input_errors.is_empty()
        && dossier.typed_quiescent
        && dossier.default_children_quiescent
        && (!has_mutations || fresh_validation);
    if !deterministic_completion_evidence_sufficient {
        return ReviewAdmissionDecision::Admit;
    }
    if !has_mutations {
        return ReviewAdmissionDecision::SkipNonMutating;
    }
    let Some(selection_input) = build_review_lens_selection_input(dossier) else {
        return ReviewAdmissionDecision::Admit;
    };
    let selected = select_review_lenses(&selection_input);
    let specialized_risk = selected
        .as_slice()
        .iter()
        .any(|lens| *lens != BEHAVIORAL_LENS);
    let mut paths = dossier
        .review_lens_selection_facts
        .task_mutation_paths
        .clone();
    paths.extend(
        dossier
            .review_lens_selection_facts
            .child_mutation_paths
            .iter()
            .cloned(),
    );
    paths.sort();
    paths.dedup();
    if !paths.is_empty()
        && paths.iter().all(|path| is_documentation_path(path))
        && !specialized_risk
    {
        return ReviewAdmissionDecision::SkipDocumentationOnly;
    }
    if fresh_validation && !specialized_risk && !paths.is_empty() {
        return ReviewAdmissionDecision::SkipFreshLowRisk;
    }
    ReviewAdmissionDecision::Admit
}

fn review_stability_blocker_reasons(dossier: &CompletionReviewDossier) -> Vec<String> {
    let mut reasons = dossier.authoritative_input_errors.clone();
    if dossier.source_capture_failed {
        reasons.push("a user source could not be durably captured".to_string());
    }
    if dossier
        .sources
        .iter()
        .any(|source| source.availability != UserSourceAvailability::Available)
        || dossier
            .source_mappings
            .values()
            .any(|mapping| matches!(mapping, SourceMapping::UnavailableOrTruncated))
    {
        reasons.push("user source evidence is unavailable or truncated".to_string());
    }
    if !dossier.typed_quiescent {
        reasons.push("typed task mutations are not quiescent".to_string());
    }
    if !dossier.default_children_quiescent {
        reasons.push("default child task mutations are not quiescent".to_string());
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

fn has_pending_review_lineage(dossier: &CompletionReviewDossier) -> bool {
    matches!(
        dossier.cycle_phase,
        Some(
            CompletionReviewCyclePhase::CorrectionPending
                | CompletionReviewCyclePhase::RereviewPending
                | CompletionReviewCyclePhase::InitialReviewPending
                | CompletionReviewCyclePhase::ClassificationPending
        )
    )
}

fn reviewer_execution_contract(config: &Config) -> ReviewerExecutionContract {
    let disabled_features = REVIEWER_DISABLED_FEATURES
        .iter()
        .map(|feature| (format!("{feature:?}"), config.features.enabled(*feature)))
        .collect::<Vec<_>>();
    ReviewerExecutionContract {
        contract_version: REVIEWER_EXECUTION_CONTRACT_VERSION,
        reviewer_model: config.model.clone().unwrap_or_default(),
        reviewer_provider: config.model_provider_id.clone(),
        reasoning_configuration: format!(
            "effort={:?};summary={:?}",
            config.model_reasoning_effort, config.model_reasoning_summary
        ),
        reviewer_prompt_hash: stable_hash(&REVIEWER_BASE_INSTRUCTIONS),
        output_schema_version: REVIEW_OUTPUT_SCHEMA_VERSION,
        tool_capability_hash: stable_hash(&json!({
            "permission_profile": "read_only",
            "approval_policy": "never",
            "web_search": "disabled",
            "mcp_servers": "none",
            "one_shot": true,
        })),
        source_classification_contract_version:
            crate::task_evidence::SOURCE_CLASSIFICATION_CONTRACT_VERSION,
        relationship_resolver_contract_version:
            crate::task_evidence::RELATIONSHIP_RESOLVER_CONTRACT_VERSION,
        review_feature_hash: stable_hash(&disabled_features),
    }
}

pub(crate) fn completion_review_configuration_identity(turn_context: &TurnContext) -> String {
    stable_hash(&serde_json::json!({
        "policy": "supplemental",
        "feature_enabled": turn_context
            .config
            .features
            .enabled(Feature::TaskCompletionReviewer),
        "model": turn_context.config.model,
        "provider": turn_context.config.model_provider_id,
        "reasoning_effort": turn_context.config.model_reasoning_effort,
        "reasoning_summary": turn_context.config.model_reasoning_summary,
        "output_contract": REVIEWER_EXECUTION_CONTRACT_VERSION,
        "prompt": stable_hash(&REVIEWER_BASE_INSTRUCTIONS),
    }))
}

fn review_attempt_identity(
    attempt_kind: CompletionReviewAttemptKind,
    dossier: &CompletionReviewDossier,
    bounded_dossier: &str,
    reviewer_contract: &ReviewerExecutionContract,
) -> ReviewAttemptIdentity {
    let bounded_dossier_hash = stable_hash(&json!({
        "contract": REVIEW_DOSSIER_CONTRACT_VERSION,
        "rendered": bounded_dossier,
    }));
    let reviewer_contract_hash = stable_hash(reviewer_contract);
    let value = stable_hash(&json!({
        "attempt_kind": attempt_kind,
        "implementation_identity_hash": dossier.implementation_identity_hash,
        "requirement_manifest_hash": dossier.requirement_manifest_hash,
        "bounded_dossier_hash": bounded_dossier_hash,
        "reviewer_contract_hash": reviewer_contract_hash,
    }));
    ReviewAttemptIdentity {
        value,
        reviewer_contract_hash,
        bounded_dossier_hash,
    }
}

pub(crate) async fn coordinate_completion_review(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    turn_evidence: &CompletionReviewTurnEvidence,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    state.record_current_phase(turn_context.turn_timing_state.as_ref());
    let outcome = coordinate_completion_review_inner(
        sess,
        turn_context,
        cancellation_token,
        turn_evidence,
        candidate_completion,
        state,
    )
    .await;
    state.record_current_phase(turn_context.turn_timing_state.as_ref());
    outcome
}

async fn coordinate_completion_review_inner(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    turn_evidence: &CompletionReviewTurnEvidence,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    if matches!(
        &turn_context.session_source,
        SessionSource::SubAgent(SubAgentSource::Review)
    ) {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    if !sess.services.task_evidence.allows_kd4_completion() {
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    if state.phase == TurnReviewPhase::Terminal {
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    if !turn_context
        .config
        .features
        .enabled(Feature::TaskCompletionReviewer)
    {
        let dossier = review_dossier(sess, candidate_completion).await;
        let obligation = dossier
            .as_ref()
            .map(resolve_disabled_review_requirement)
            .unwrap_or(ReviewObligationMode::Disabled);
        let synchronized = matches!(
            sess.services
                .task_evidence
                .synchronize_completion_review_obligation(CompletionReviewObligationInput {
                    mode: obligation.name().to_string(),
                    requirement_ids: obligation.requirement_ids(),
                    obligation_hash: obligation.hash(),
                    required_attempt_identity: None,
                })
                .await,
            AtomicReviewTransition::Persisted(())
        );
        if synchronized
            && obligation.is_mandatory()
            && let Some(dossier) = dossier.as_ref()
        {
            record_review_infrastructure(
                sess,
                turn_context,
                dossier,
                &obligation,
                ReviewAdmissionDecision::Admit,
                None,
                ReviewFailureCategory::UnsupportedConfiguration,
                None,
            )
            .await;
        }
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            advisory: sess.services.task_evidence.finalization_advisory().await,
            ..Default::default()
        });
    }

    let mut candidate_restart_count = 0u8;
    loop {
        let Some(mut dossier) = review_dossier(sess, candidate_completion).await else {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        };
        if matches!(
            dossier.cycle_phase,
            Some(
                CompletionReviewCyclePhase::TerminalPartial
                    | CompletionReviewCyclePhase::TerminalBlocked
                    | CompletionReviewCyclePhase::Closed
            )
        ) {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome {
                advisory: sess.services.task_evidence.finalization_advisory().await,
                ..Default::default()
            });
        }
        if dossier.cycle_phase == Some(CompletionReviewCyclePhase::ProvisionalClean) {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome {
                provisional_clean: true,
                ..Default::default()
            });
        }
        let pending_lineage = has_pending_review_lineage(&dossier);
        let stability_reasons = if pending_lineage {
            Vec::new()
        } else {
            review_stability_blocker_reasons(&dossier)
        };
        if !stability_reasons.is_empty() {
            let obligation = if dossier.mappings_classified {
                match resolve_review_obligation(&dossier) {
                    ReviewObligationResolution::Resolved(obligation) => obligation,
                    ReviewObligationResolution::NeedsObligationMaterialization => {
                        ReviewObligationMode::Supplemental
                    }
                }
            } else {
                ReviewObligationMode::Supplemental
            };
            record_review_not_admitted_correctness(
                sess,
                turn_context,
                &obligation,
                &stability_reasons,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome {
                partial_reasons: stability_reasons,
                ..Default::default()
            });
        }
        if !pending_lineage && !user_sources_still_current(&dossier).await {
            let reasons = vec!["user source evidence changed before review admission".to_string()];
            record_review_not_admitted_correctness(
                sess,
                turn_context,
                &ReviewObligationMode::Supplemental,
                &reasons,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome {
                partial_reasons: reasons,
                ..Default::default()
            });
        }

        if !dossier.mappings_classified {
            let materialization =
                match materialize_sources(sess, turn_context, cancellation_token, &dossier, None)
                    .await?
                {
                    Ok(materialization) => materialization,
                    Err(failure) => {
                        record_review_infrastructure(
                            sess,
                            turn_context,
                            &dossier,
                            &ReviewObligationMode::Supplemental,
                            ReviewAdmissionDecision::Admit,
                            None,
                            failure,
                            None,
                        )
                        .await;
                        state.phase = TurnReviewPhase::Terminal;
                        return Ok(CompletionReviewCoordinatorOutcome::default());
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
                        state.phase = TurnReviewPhase::Terminal;
                        return Ok(CompletionReviewCoordinatorOutcome::default());
                    };
                    dossier = fresh;
                }
                AtomicReviewTransition::Superseded => {
                    if candidate_restart_count == 0 {
                        candidate_restart_count = 1;
                        continue;
                    }
                    state.phase = TurnReviewPhase::Terminal;
                    return Ok(superseded_persistence_outcome());
                }
                AtomicReviewTransition::Failed => {
                    record_review_infrastructure(
                        sess,
                        turn_context,
                        &dossier,
                        &ReviewObligationMode::Supplemental,
                        ReviewAdmissionDecision::Admit,
                        None,
                        ReviewFailureCategory::Persistence,
                        None,
                    )
                    .await;
                    state.phase = TurnReviewPhase::Terminal;
                    return Ok(partial_outcome(ReviewFailureCategory::Persistence));
                }
            }
        }

        let obligation = match resolve_review_obligation(&dossier) {
            ReviewObligationResolution::Resolved(obligation) => obligation,
            ReviewObligationResolution::NeedsObligationMaterialization => {
                record_review_infrastructure(
                    sess,
                    turn_context,
                    &dossier,
                    &ReviewObligationMode::Supplemental,
                    ReviewAdmissionDecision::Admit,
                    None,
                    ReviewFailureCategory::InvalidDossier,
                    None,
                )
                .await;
                state.phase = TurnReviewPhase::Terminal;
                return Ok(CompletionReviewCoordinatorOutcome::default());
            }
        };
        let obligation_input = CompletionReviewObligationInput {
            mode: obligation.name().to_string(),
            requirement_ids: obligation.requirement_ids(),
            obligation_hash: obligation.hash(),
            required_attempt_identity: None,
        };
        match sess
            .services
            .task_evidence
            .synchronize_completion_review_obligation(obligation_input)
            .await
        {
            AtomicReviewTransition::Persisted(()) => {}
            AtomicReviewTransition::Superseded => {
                if candidate_restart_count == 0 {
                    candidate_restart_count = 1;
                    continue;
                }
                state.phase = TurnReviewPhase::Terminal;
                return Ok(superseded_persistence_outcome());
            }
            AtomicReviewTransition::Failed => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
        let Some(fresh) = review_dossier(sess, candidate_completion).await else {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        };
        dossier = fresh;

        let pending_lineage = has_pending_review_lineage(&dossier);
        let admission = if pending_lineage {
            if matches!(
                &turn_context.session_source,
                SessionSource::SubAgent(SubAgentSource::Review)
            ) {
                ReviewAdmissionDecision::RejectSelfReview
            } else {
                ReviewAdmissionDecision::Admit
            }
        } else {
            review_admission_decision(turn_context, &dossier, &obligation, turn_evidence)
        };
        if admission == ReviewAdmissionDecision::RejectSelfReview {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                &obligation,
                admission,
                None,
                ReviewFailureCategory::SelfReviewProhibited,
                None,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        if admission != ReviewAdmissionDecision::Admit {
            record_review_skip(sess, turn_context, &obligation, admission).await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome {
                advisory: sess.services.task_evidence.finalization_advisory().await,
                ..Default::default()
            });
        }
        if dossier.sources.iter().any(|source| {
            source.availability != UserSourceAvailability::Available
                || matches!(
                    dossier.source_mappings.get(&source.source_id),
                    Some(SourceMapping::UnavailableOrTruncated)
                )
        }) {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                &obligation,
                admission,
                None,
                ReviewFailureCategory::InputUnavailable,
                None,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }

        let kind = match dossier.cycle_phase {
            Some(CompletionReviewCyclePhase::RereviewPending) => ReviewerRequestKind::Rereview,
            Some(CompletionReviewCyclePhase::CorrectionPending) => {
                return resume_correction(
                    sess,
                    turn_context,
                    cancellation_token,
                    candidate_completion,
                    state,
                    dossier,
                    &obligation,
                    turn_evidence,
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
            Some(CompletionReviewCyclePhase::TerminalBlocked)
            | Some(CompletionReviewCyclePhase::TerminalPartial)
            | Some(CompletionReviewCyclePhase::Closed) => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(CompletionReviewCoordinatorOutcome::default());
            }
            Some(CompletionReviewCyclePhase::InitialReviewPending)
            | Some(CompletionReviewCyclePhase::ClassificationPending)
            | None => ReviewerRequestKind::InitialReview,
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
            &obligation,
            turn_evidence,
        )
        .await?;
        if outcome.candidate_changed {
            if candidate_restart_count == 0 {
                candidate_restart_count = 1;
                state.phase = TurnReviewPhase::Ready;
                continue;
            }
            outcome.candidate_changed = false;
            state.phase = TurnReviewPhase::Terminal;
        }
        attach_lens_observation_advisories(&mut outcome, lens_observation_advisories);
        return Ok(outcome);
    }
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
    pub(crate) typed_validation_proofs: Vec<TypedValidationProofInputV1>,
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
    match store.read_workspace_events(&repo_root, event_cursor).await {
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
            let binding_evidence = collect_bounded_in_order(
                bindings.into_iter().map(|binding| {
                    let store = Arc::clone(&store);
                    async move {
                        let (task, mutations) = tokio::join!(
                            store.get_agent_task(binding.assignment_id, Some(0)),
                            store.list_mutation_evidence(
                                binding.attempt_id,
                                Some(AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT),
                            )
                        );
                        (binding, task, mutations)
                    }
                }),
                AUTHORITATIVE_BINDING_READ_CONCURRENCY,
            )
            .await;
            for (binding, task, mutations) in binding_evidence {
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
                        result
                            .typed_validation_proofs
                            .extend(authoritative_typed_validation_proofs(&task));
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
    result.typed_validation_proofs.sort_by(|left, right| {
        (&left.assignment_id, &left.attempt_id, &left.call_id).cmp(&(
            &right.assignment_id,
            &right.attempt_id,
            &right.call_id,
        ))
    });
    result.typed_validation_proofs.dedup();
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

async fn collect_bounded_in_order<T>(
    futures: impl IntoIterator<Item = impl Future<Output = T> + Send>,
    concurrency: usize,
) -> Vec<T> {
    async move {
        futures::stream::iter(futures)
            .buffered(concurrency)
            .collect()
            .await
    }
    .await
}

fn authoritative_typed_validation_proofs(task: &AgentTask) -> Vec<TypedValidationProofInputV1> {
    let Some(receipt) = task.receipt.as_ref() else {
        return Vec::new();
    };
    let attempt = &task.current_attempt;
    let workspace_epoch = task.workspace_status.epoch;
    if attempt.state != AttemptState::Completed
        || !receipt.status.is_success()
        || receipt.assignment_id != task.assignment.assignment_id
        || receipt.assignment_id != attempt.assignment_id
        || receipt.attempt_id != attempt.attempt_id
        || receipt.evidence_epoch > workspace_epoch
    {
        return Vec::new();
    }

    receipt
        .validation_call_ids
        .iter()
        .filter_map(|call_id| {
            let call = task
                .validation_calls
                .iter()
                .find(|call| call.call_id == *call_id && call.attempt_id == attempt.attempt_id)?;
            let evidence = &call.evidence;
            if call.status != ValidationCallStatus::Succeeded
                || call.proof_kind != ValidationProofKind::Focused
                || call
                    .resolved_executable
                    .as_deref()
                    .is_none_or(|path| !Path::new(path).is_absolute())
                || !evidence.is_reusable_success()
                || evidence.end_epoch != Some(receipt.evidence_epoch)
                || evidence
                    .source_evidence_epoch
                    .is_none_or(|epoch| epoch > receipt.evidence_epoch)
                || evidence.covered_manifest.is_empty()
            {
                return None;
            }
            let validation_result: codex_protocol::validation::ValidationResult = evidence
                .validation_result
                .clone()
                .and_then(|value| serde_json::from_value(value).ok())?;
            if validation_result.call_id != call.call_id
                || !validation_result.status.is_success()
                || validation_result.freshness
                    == codex_protocol::validation::ValidationFreshness::Superseded
                || validation_result.proof_key.validation_contract_version
                    != codex_protocol::validation::VALIDATION_CONTRACT_VERSION
                || validation_result.proof_key.implementation_identity
                    != evidence.implementation_identity
                || validation_result.proof_key.coverage_identity != evidence.coverage_identity
                || validation_result.proof_key.repository.is_empty()
                || validation_result.proof_key.cwd.is_empty()
                || evidence.cwd.as_deref() != Some(validation_result.proof_key.cwd.as_str())
                || validation_result.route.leaves.is_empty()
                || validation_result
                    .raw_artifact_ref
                    .as_deref()
                    .is_none_or(str::is_empty)
                || validation_result
                    .raw_artifact_sha256
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                return None;
            }
            Some(TypedValidationProofInputV1 {
                assignment_id: receipt.assignment_id.to_string(),
                attempt_id: receipt.attempt_id.to_string(),
                call_id: call.call_id.clone(),
                receipt_evidence_epoch: receipt.evidence_epoch,
                workspace_epoch,
                validation_end_epoch: evidence.end_epoch?,
                implementation_identity: evidence.implementation_identity.clone(),
                coverage_identity: evidence.coverage_identity.clone(),
                recorded_cwd: evidence.cwd.clone()?,
                retained_output_digest: evidence.retained_output_digest.clone(),
                retained_output_ref: evidence.retained_output_ref.clone()?,
                covered_manifest: evidence.covered_manifest.clone(),
                current_workspace_manifest_identity: None,
                validation_result,
            })
        })
        .collect()
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
    if failure.is_review_infrastructure() {
        return CompletionReviewCoordinatorOutcome {
            advisory: Some(format!(
                "Supplemental completion review infrastructure did not establish a review result: {}",
                failure.partial_reason()
            )),
            ..Default::default()
        };
    }
    CompletionReviewCoordinatorOutcome {
        partial_reasons: vec![failure.partial_reason().to_string()],
        ..Default::default()
    }
}

fn superseded_persistence_outcome() -> CompletionReviewCoordinatorOutcome {
    CompletionReviewCoordinatorOutcome {
        candidate_changed: true,
        partial_reasons: vec![
            "completion review persistence was superseded by a concurrent authoritative update"
                .to_string(),
        ],
        ..Default::default()
    }
}

async fn record_review_skip(
    sess: &Session,
    turn_context: &TurnContext,
    obligation: &ReviewObligationMode,
    admission: ReviewAdmissionDecision,
) {
    let _ = sess
        .services
        .task_evidence
        .record_completion_review_audit_with_measurements(
            &turn_context.sub_id,
            "risk_skipped",
            None,
            Vec::new(),
            false,
            CompletionReviewAuditMeasurements {
                obligation_mode: obligation.name().to_string(),
                obligation_hash: obligation.hash(),
                admission_result: admission.as_str().to_string(),
                preflight_result: "skipped_before_capacity".to_string(),
                mandatory_proof_state: if obligation.is_mandatory() {
                    "missing".to_string()
                } else {
                    "not_required".to_string()
                },
                ..Default::default()
            },
        )
        .await;
}

async fn record_review_not_admitted_correctness(
    sess: &Session,
    turn_context: &TurnContext,
    obligation: &ReviewObligationMode,
    reasons: &[String],
) {
    turn_context
        .turn_timing_state
        .record_review_prevented_by_correctness();
    let _ = sess
        .services
        .task_evidence
        .record_completion_review_audit_with_measurements(
            &turn_context.sub_id,
            "not_admitted_correctness",
            Some("correctness_gate_failed"),
            reasons.to_vec(),
            false,
            CompletionReviewAuditMeasurements {
                obligation_mode: obligation.name().to_string(),
                obligation_hash: obligation.hash(),
                admission_result: ReviewAdmissionDecision::NotAdmittedCorrectness
                    .as_str()
                    .to_string(),
                preflight_result: "prevented_before_reviewer_construction".to_string(),
                mandatory_proof_state: if obligation.is_mandatory() {
                    "missing".to_string()
                } else {
                    "not_required".to_string()
                },
                ..Default::default()
            },
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn record_review_infrastructure(
    sess: &Session,
    turn_context: &TurnContext,
    dossier: &CompletionReviewDossier,
    obligation: &ReviewObligationMode,
    admission: ReviewAdmissionDecision,
    attempt: Option<&ReviewAttemptIdentity>,
    failure: ReviewFailureCategory,
    execution: Option<&ReviewerExecution>,
) {
    if dossier.active_cycle_id.is_some() {
        let _ = sess
            .services
            .task_evidence
            .abandon_completion_review_cycle(dossier)
            .await;
    }
    let execution = execution.map(|execution| {
        (
            execution.elapsed_millis,
            execution.logical_generations,
            execution.physical_requests,
            execution.tool_calls,
        )
    });
    let (elapsed_millis, logical_generations, physical_requests, tool_calls) =
        execution.unwrap_or_default();
    let retry_disposition = match failure.class() {
        ReviewFailureClass::Deterministic => "suppress_until_identity_changes",
        ReviewFailureClass::Availability => "bounded_retry_or_availability_revision_required",
        ReviewFailureClass::ExecutionBounded => "suppress_until_runtime_or_identity_changes",
    };
    let _ = sess
        .services
        .task_evidence
        .record_completion_review_audit_with_measurements(
            &turn_context.sub_id,
            "review_infrastructure_failed",
            Some(failure.as_str()),
            Vec::new(),
            false,
            CompletionReviewAuditMeasurements {
                obligation_mode: obligation.name().to_string(),
                obligation_hash: obligation.hash(),
                admission_result: admission.as_str().to_string(),
                preflight_result: failure.as_str().to_string(),
                attempt_identity: attempt
                    .map(|attempt| attempt.value.clone())
                    .unwrap_or_default(),
                reviewer_contract_hash: attempt
                    .map(|attempt| attempt.reviewer_contract_hash.clone())
                    .unwrap_or_default(),
                failure_class: failure.class().as_str().to_string(),
                retry_disposition: retry_disposition.to_string(),
                elapsed_millis,
                logical_generations,
                physical_requests,
                tool_calls,
                mandatory_proof_state: if obligation.is_mandatory() {
                    "missing".to_string()
                } else {
                    "not_required".to_string()
                },
                review_infrastructure_caused_partial: false,
                ..Default::default()
            },
        )
        .await;
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
    obligation: &ReviewObligationMode,
    turn_evidence: &CompletionReviewTurnEvidence,
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    let mut preflight_timer = ReviewTelemetryTimer::start(
        Arc::clone(&turn_context.turn_timing_state),
        ReviewTelemetryPhase::Preflight,
    );
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
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            None,
            ReviewFailureCategory::InputUnavailable,
            None,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    let selected_lenses = select_review_lenses(&selection_input);
    let Some(frozen_original_findings_identity) =
        original_findings_identity(&dossier.original_findings)
    else {
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            None,
            ReviewFailureCategory::InputUnavailable,
            None,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    if !user_sources_still_current(&dossier).await {
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            None,
            ReviewFailureCategory::SourceDrift,
            None,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    let (inputs, bounded_dossier) = match build_final_reviewer_inputs(
        &dossier,
        kind,
        &selected_lenses,
        obligation,
        turn_evidence,
    )
    .await
    {
        Ok(inputs) => inputs,
        Err(failure) => {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                obligation,
                ReviewAdmissionDecision::Admit,
                None,
                failure,
                None,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
    };
    let requires_images = inputs.iter().any(|input| {
        matches!(
            input,
            UserInput::Image { .. } | UserInput::LocalImage { .. }
        )
    });
    let subconfig = match build_reviewer_config(turn_context, requires_images).await {
        Ok(config) => config,
        Err(()) => {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                obligation,
                ReviewAdmissionDecision::Admit,
                None,
                ReviewFailureCategory::UnsupportedConfiguration,
                None,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
    };
    let schema = completion_review_output_schema(&selected_lenses);
    let reviewer_contract = reviewer_execution_contract(&subconfig);
    let attempt_identity =
        review_attempt_identity(attempt_kind, &dossier, &bounded_dossier, &reviewer_contract);
    let preflight_implementation_identity = dossier.implementation_identity_hash.clone();
    let preflight_dossier_snapshot = dossier.dossier_snapshot_id.clone();
    let preflight_manifest_hash = dossier.requirement_manifest_hash.clone();
    match sess
        .services
        .task_evidence
        .synchronize_completion_review_obligation(CompletionReviewObligationInput {
            mode: obligation.name().to_string(),
            requirement_ids: obligation.requirement_ids(),
            obligation_hash: obligation.hash(),
            required_attempt_identity: obligation
                .is_mandatory()
                .then(|| attempt_identity.value.clone()),
        })
        .await
    {
        AtomicReviewTransition::Persisted(()) => {}
        AtomicReviewTransition::Superseded => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(superseded_persistence_outcome());
        }
        AtomicReviewTransition::Failed => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        }
    }
    let Some(mut dossier) = review_dossier(sess, candidate_completion).await else {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    if dossier.implementation_identity_hash != preflight_implementation_identity
        || dossier.dossier_snapshot_id != preflight_dossier_snapshot
        || dossier.requirement_manifest_hash != preflight_manifest_hash
    {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            candidate_changed: true,
            ..Default::default()
        });
    }
    match sess
        .services
        .task_evidence
        .prior_completion_review_attempt(&attempt_identity.value)
        .await
    {
        Some(PriorCompletionReviewAttempt::Clean) => {
            let _ = sess
                .services
                .task_evidence
                .reuse_completion_review_clean_proof(&attempt_identity.value)
                .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        Some(
            PriorCompletionReviewAttempt::Actionable
            | PriorCompletionReviewAttempt::DeterministicInfrastructure,
        ) => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        None => {}
    }
    if !sess.completion_review_capacity_available() {
        let condition = "review_capacity_unavailable";
        if sess
            .services
            .task_evidence
            .reviewer_infrastructure_memo_matches(
                &dossier.implementation_identity_hash,
                &dossier.dossier_snapshot_id,
                &attempt_identity.reviewer_contract_hash,
                condition,
            )
            .await
        {
            turn_context
                .turn_timing_state
                .record_reviewer_infrastructure_memo_hit();
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            Some(&attempt_identity),
            ReviewFailureCategory::Capacity,
            None,
        )
        .await;
        let _ = sess
            .services
            .task_evidence
            .record_reviewer_infrastructure_memo(
                dossier.implementation_identity_hash.clone(),
                dossier.dossier_snapshot_id.clone(),
                attempt_identity.reviewer_contract_hash.clone(),
                condition.to_string(),
                ReviewFailureCategory::Capacity.as_str().to_string(),
            )
            .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    let Some(review_permit) = sess.try_acquire_completion_review_slot() else {
        let condition = "review_capacity_saturated";
        if sess
            .services
            .task_evidence
            .reviewer_infrastructure_memo_matches(
                &dossier.implementation_identity_hash,
                &dossier.dossier_snapshot_id,
                &attempt_identity.reviewer_contract_hash,
                condition,
            )
            .await
        {
            turn_context
                .turn_timing_state
                .record_reviewer_infrastructure_memo_hit();
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            Some(&attempt_identity),
            ReviewFailureCategory::Capacity,
            None,
        )
        .await;
        let _ = sess
            .services
            .task_evidence
            .record_reviewer_infrastructure_memo(
                dossier.implementation_identity_hash.clone(),
                dossier.dossier_snapshot_id.clone(),
                attempt_identity.reviewer_contract_hash.clone(),
                condition.to_string(),
                ReviewFailureCategory::Capacity.as_str().to_string(),
            )
            .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    let review_cancellation = CancellationToken::new();
    let prepared = match PreparedCodexOneShot::start(
        subconfig,
        Arc::clone(&sess.services.auth_manager),
        Arc::clone(&sess.services.models_manager),
        Arc::clone(sess),
        Arc::clone(turn_context),
        review_cancellation.clone(),
        SubAgentSource::Review,
        None,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(_) => {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                obligation,
                ReviewAdmissionDecision::Admit,
                Some(&attempt_identity),
                ReviewFailureCategory::SpawnModel,
                None,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
    };
    let Some(revalidated) = review_dossier(sess, candidate_completion).await else {
        prepared.shutdown().await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };
    let revalidated_attempt = review_attempt_identity(
        attempt_kind,
        &revalidated,
        &bounded_dossier,
        &reviewer_contract,
    );
    if revalidated.implementation_identity_hash != dossier.implementation_identity_hash
        || revalidated.dossier_snapshot_id != dossier.dossier_snapshot_id
        || revalidated.requirement_manifest_hash != dossier.requirement_manifest_hash
        || revalidated_attempt.value != attempt_identity.value
        || !user_sources_still_current(&revalidated).await
    {
        prepared.shutdown().await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            candidate_changed: true,
            ..Default::default()
        });
    }
    dossier = revalidated;
    if dossier.active_cycle_id.is_none() {
        match sess
            .services
            .task_evidence
            .begin_completion_review_cycle(&dossier)
            .await
        {
            AtomicReviewTransition::Persisted(_) => {
                let Some(fresh) = review_dossier(sess, candidate_completion).await else {
                    prepared.shutdown().await;
                    state.phase = TurnReviewPhase::Terminal;
                    return Ok(CompletionReviewCoordinatorOutcome::default());
                };
                dossier = fresh;
            }
            AtomicReviewTransition::Superseded => {
                prepared.shutdown().await;
                state.phase = TurnReviewPhase::Terminal;
                return Ok(superseded_persistence_outcome());
            }
            AtomicReviewTransition::Failed => {
                prepared.shutdown().await;
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
    }
    preflight_timer.finish();
    let mut review_timer = ReviewTelemetryTimer::start(
        Arc::clone(&turn_context.turn_timing_state),
        ReviewTelemetryPhase::Review,
    );
    let execution = run_prepared_reviewer_with_deadline(
        prepared,
        inputs,
        schema,
        kind,
        review_cancellation,
        cancellation_token,
    )
    .await?;
    review_timer.finish();
    drop(review_permit);
    if execution.payload.is_none() {
        let failure = execution
            .failures
            .first()
            .copied()
            .unwrap_or(ReviewFailureCategory::MalformedOutput);
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            Some(&attempt_identity),
            failure,
            Some(&execution),
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    let measured_execution = ReviewerExecution {
        payload: None,
        failures: execution.failures.clone(),
        elapsed_millis: execution.elapsed_millis,
        logical_generations: execution.logical_generations,
        physical_requests: execution.physical_requests,
        tool_calls: execution.tool_calls,
    };
    let Some(ReviewerPayload::Review(output)) = execution.payload else {
        unreachable!("final-review execution can only contain a review payload")
    };
    let Some(validated) = validate_review_output(
        &dossier,
        output,
        matches!(kind, ReviewerRequestKind::Rereview),
        &selected_lenses,
    ) else {
        record_review_infrastructure(
            sess,
            turn_context,
            &dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            Some(&attempt_identity),
            ReviewFailureCategory::MalformedOutput,
            Some(&measured_execution),
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    };

    let Some(fresh_dossier) = review_dossier(sess, candidate_completion).await else {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
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
        record_review_infrastructure(
            sess,
            turn_context,
            &fresh_dossier,
            obligation,
            ReviewAdmissionDecision::Admit,
            Some(&attempt_identity),
            ReviewFailureCategory::Persistence,
            Some(&measured_execution),
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            candidate_changed: true,
            ..Default::default()
        });
    }
    let dossier = fresh_dossier;

    if !validated.manifest_gaps.is_empty() {
        if gap_reconstructed || dossier.manifest_gap_reconstructed {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                obligation,
                ReviewAdmissionDecision::Admit,
                Some(&attempt_identity),
                ReviewFailureCategory::RepeatedManifestGap,
                Some(&measured_execution),
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
        }
        let Some(local_classifications) =
            source_local_classifications_with_manifest_gaps(&dossier, &validated.manifest_gaps)
        else {
            record_review_infrastructure(
                sess,
                turn_context,
                &dossier,
                obligation,
                ReviewAdmissionDecision::Admit,
                Some(&attempt_identity),
                ReviewFailureCategory::MalformedOutput,
                Some(&measured_execution),
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
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
                record_review_infrastructure(
                    sess,
                    turn_context,
                    &dossier,
                    obligation,
                    ReviewAdmissionDecision::Admit,
                    Some(&attempt_identity),
                    failure,
                    Some(&measured_execution),
                )
                .await;
                state.phase = TurnReviewPhase::Terminal;
                return Ok(CompletionReviewCoordinatorOutcome::default());
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
            &attempt_identity,
        )
        .await
        {
            AtomicReviewTransition::Persisted(_) => {}
            AtomicReviewTransition::Superseded => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(superseded_persistence_outcome());
            }
            AtomicReviewTransition::Failed => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
        let Some(rebuilt) = review_dossier(sess, candidate_completion).await else {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(CompletionReviewCoordinatorOutcome::default());
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
            obligation,
            turn_evidence,
        ))
        .await;
    }
    if validated.review_clean {
        match persist_validated_attempt(
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
            &attempt_identity,
        )
        .await
        {
            AtomicReviewTransition::Persisted(_) => {}
            AtomicReviewTransition::Superseded => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(superseded_persistence_outcome());
            }
            AtomicReviewTransition::Failed => {
                state.phase = TurnReviewPhase::Terminal;
                return Ok(partial_outcome(ReviewFailureCategory::Persistence));
            }
        }
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome {
            provisional_clean: true,
            ..Default::default()
        });
    }
    if matches!(kind, ReviewerRequestKind::Rereview) || dossier.correction_consumed {
        let transition = persist_validated_attempt(
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
            &attempt_identity,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(match transition {
            AtomicReviewTransition::Persisted(_) => CompletionReviewCoordinatorOutcome::default(),
            AtomicReviewTransition::Superseded => superseded_persistence_outcome(),
            AtomicReviewTransition::Failed => partial_outcome(ReviewFailureCategory::Persistence),
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
    let repair = build_repair_item(&dossier, &preview_findings);
    let Some((repair_item, repair_payload)) = repair else {
        let transition = persist_validated_attempt(
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
            &attempt_identity,
        )
        .await;
        state.phase = TurnReviewPhase::Terminal;
        return Ok(match transition {
            AtomicReviewTransition::Persisted(_) => CompletionReviewCoordinatorOutcome::default(),
            AtomicReviewTransition::Superseded => superseded_persistence_outcome(),
            AtomicReviewTransition::Failed => partial_outcome(ReviewFailureCategory::Persistence),
        });
    };
    let recorded = match persist_validated_attempt(
        sess,
        &dossier,
        attempt_kind,
        parent_review_id,
        validated,
        Some(repair_payload),
        None,
        None,
        gap_reconstructed,
        lens_observation_advisories,
        &attempt_identity,
    )
    .await
    {
        AtomicReviewTransition::Persisted(recorded) => recorded,
        AtomicReviewTransition::Superseded => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(superseded_persistence_outcome());
        }
        AtomicReviewTransition::Failed => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        }
    };
    if recorded.review_id != preview_review_id || recorded.findings != preview_findings {
        state.phase = TurnReviewPhase::Terminal;
        return Ok(CompletionReviewCoordinatorOutcome::default());
    }
    sess.record_response_item_and_emit_turn_item(turn_context, repair_item)
        .await;
    state.phase = TurnReviewPhase::CorrectionInjected;
    Ok(CompletionReviewCoordinatorOutcome {
        repair_injected: true,
        ..Default::default()
    })
}

// A correction resumes the same coordinator transaction and therefore needs
// the complete session, turn, cancellation, review, and evidence context.
#[allow(clippy::too_many_arguments)]
async fn resume_correction(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    cancellation_token: &CancellationToken,
    candidate_completion: Option<&str>,
    state: &mut CompletionReviewState,
    dossier: CompletionReviewDossier,
    obligation: &ReviewObligationMode,
    turn_evidence: &CompletionReviewTurnEvidence,
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
    match persist_correction_evidence(sess, &dossier, &initial_review_id).await {
        AtomicReviewTransition::Persisted(_) => {}
        AtomicReviewTransition::Superseded => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(superseded_persistence_outcome());
        }
        AtomicReviewTransition::Failed => {
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::Persistence));
        }
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
        obligation,
        turn_evidence,
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
    attempt_identity: &ReviewAttemptIdentity,
) -> AtomicReviewTransition<RecordedReviewAttempt> {
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
        attempt_identity: attempt_identity.value.clone(),
        reviewer_contract_hash: attempt_identity.reviewer_contract_hash.clone(),
    };
    let transition = if input.manifest_gaps.is_empty() {
        if source_materialization.is_some() {
            return AtomicReviewTransition::Failed;
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
                match source_materialization {
                    Some(source_materialization) => source_materialization,
                    None => return AtomicReviewTransition::Failed,
                },
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
            AtomicReviewTransition::Persisted(recorded)
        }
        AtomicReviewTransition::Superseded => AtomicReviewTransition::Superseded,
        AtomicReviewTransition::Failed => AtomicReviewTransition::Failed,
    }
}

async fn persist_correction_evidence(
    sess: &Session,
    dossier: &CompletionReviewDossier,
    initial_review_id: &str,
) -> AtomicReviewTransition<RecordedReviewAttempt> {
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
                attempt_identity: String::new(),
                reviewer_contract_hash: String::new(),
            },
        )
        .await
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
    if findings.is_empty() {
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
    use crate::config::ConfigBuilder;
    use crate::task_evidence::CurrentRepairSnapshot;
    use chrono::Utc;
    use codex_agent_task_store::AcceptanceCriterion;
    use codex_agent_task_store::AgentReceipt;
    use codex_agent_task_store::AgentRole;
    use codex_agent_task_store::AgentStatusClaim;
    use codex_agent_task_store::Assignment;
    use codex_agent_task_store::AssignmentId;
    use codex_agent_task_store::Attempt;
    use codex_agent_task_store::AttemptId;
    use codex_agent_task_store::CapabilityProfile;
    use codex_agent_task_store::CriterionResult;
    use codex_agent_task_store::CriterionStatus;
    use codex_agent_task_store::ValidationCall;
    use codex_agent_task_store::ValidationEvidence;
    use codex_agent_task_store::WorkspaceManifestEntry;
    use codex_agent_task_store::WorkspaceStrategy;
    use codex_agent_task_store::WorkspaceTaskStatus;
    use codex_protocol::plan_tool::ValidationRoute;
    use codex_protocol::plan_tool::ValidationRouteLeaf;
    use codex_protocol::plan_tool::ValidationRouteOrdering;
    use codex_protocol::protocol::TaskCompletionGate;
    use codex_protocol::validation::ValidationFreshness;
    use codex_protocol::validation::ValidationProofKey;
    use codex_protocol::validation::ValidationResult;
    use codex_protocol::validation::ValidationTerminalStatus;
    use sha2::Digest;
    use sha2::Sha256;
    use tempfile::tempdir;

    #[tokio::test]
    async fn authoritative_binding_reads_are_concurrent_and_ordered() {
        async fn wait_and_return(
            barrier: Arc<tokio::sync::Barrier>,
            value: &'static str,
        ) -> &'static str {
            barrier.wait().await;
            value
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let second_barrier = Arc::clone(&barrier);

        let values = tokio::time::timeout(
            Duration::from_millis(200),
            collect_bounded_in_order(
                [
                    wait_and_return(first_barrier, "first"),
                    wait_and_return(second_barrier, "second"),
                ],
                AUTHORITATIVE_BINDING_READ_CONCURRENCY,
            ),
        )
        .await
        .expect("independent binding reads should overlap");

        assert_eq!(values, vec!["first", "second"]);
    }

    fn completed_typed_task_with_structured_validation() -> AgentTask {
        let assignment_id = AssignmentId::new();
        let attempt_id = AttemptId::new();
        let now = Utc::now();
        let workspace_epoch = 7;
        let call_id = "typed-validation".to_string();
        let implementation_identity = "typed-implementation".to_string();
        let coverage_identity = "typed-coverage".to_string();
        let repository = "C:/repo".to_string();
        let validation_result = ValidationResult {
            proof_key: ValidationProofKey {
                repository: repository.clone(),
                cwd: repository.clone(),
                canonical_route_hash: "typed-route".to_string(),
                implementation_identity: implementation_identity.clone(),
                coverage_identity: coverage_identity.clone(),
                environment_identity: "typed-environment".to_string(),
                toolchain_identity: "typed-toolchain".to_string(),
                configuration_identity: "typed-configuration".to_string(),
                validation_contract_version:
                    codex_protocol::validation::VALIDATION_CONTRACT_VERSION,
            },
            route: ValidationRoute {
                leaves: vec![ValidationRouteLeaf {
                    argv: vec!["cargo".to_string(), "test".to_string(), "typed".to_string()],
                    uncertainty: "typed validation".to_string(),
                    covered_paths: vec!["codex-rs/core/src/task_evidence.rs".to_string()],
                    covered_contracts: vec!["typed-child-final-proof".to_string()],
                    timeout_ms: 120_000,
                    semantic_timeout: false,
                }],
                ordering: ValidationRouteOrdering::RunAll,
            },
            call_id: call_id.clone(),
            process_id: None,
            status: ValidationTerminalStatus::Succeeded,
            duration_ms: 17,
            summary: Some("typed validation passed".to_string()),
            failure_excerpt: None,
            raw_artifact_ref: Some("artifact://typed-raw".to_string()),
            raw_artifact_sha256: Some("typed-raw-sha256".to_string()),
            freshness: ValidationFreshness::Executed,
        };
        AgentTask {
            assignment: Assignment {
                assignment_id,
                root_session_id: "root-session".to_string(),
                admission_origin: codex_agent_task_store::AssignmentAdmissionOrigin::Typed,
                repository_id: "repository".to_string(),
                workspace_id: "workspace".to_string(),
                role: AgentRole::Worker,
                capability_profile: CapabilityProfile::ScopedSourceWrite,
                objective: "complete typed validation".to_string(),
                acceptance_criteria: vec![AcceptanceCriterion {
                    id: "criterion".to_string(),
                    text: "typed validation passes".to_string(),
                }],
                read_scope: Vec::new(),
                write_scope: Vec::new(),
                stop_condition: "task complete".to_string(),
                dependencies: Vec::new(),
                risk_hints: Vec::new(),
                required_evidence: Vec::new(),
                prohibited_changes: Vec::new(),
                contract_claims: Vec::new(),
                workspace_strategy: WorkspaceStrategy::Shared,
                start_epoch: workspace_epoch,
                relation: None,
                task_capsule: None,
                architecture_contract_ref: None,
                integration_plan: codex_agent_task_store::IntegrationPlan::SingleWriter,
                created_at: now,
            },
            current_attempt: Attempt {
                attempt_id,
                assignment_id,
                ordinal: 0,
                amendment: None,
                state: AttemptState::Completed,
                created_at: now,
                sealed_at: Some(now),
            },
            gates: Vec::new(),
            receipt: Some(AgentReceipt {
                assignment_id,
                attempt_id,
                status: AgentStatusClaim::Completed,
                summary: "completed".to_string(),
                criterion_results: vec![CriterionResult {
                    criterion_id: "criterion".to_string(),
                    status: CriterionStatus::Passed,
                    evidence: None,
                }],
                declared_changes: Vec::new(),
                validation_call_ids: vec![call_id.clone()],
                blockers: Vec::new(),
                risks: Vec::new(),
                next_action: None,
                architecture_contract: None,
                evidence_epoch: workspace_epoch,
                evidence_manifest_hash: "typed-manifest".to_string(),
                sealed_at: now,
            }),
            validation_calls: vec![ValidationCall {
                call_id,
                attempt_id,
                command_summary: "focused validation".to_string(),
                resolved_executable: Some(
                    std::fs::canonicalize(
                        std::env::current_exe().expect("current test executable"),
                    )
                    .expect("current test executable canonicalizes")
                    .to_string_lossy()
                    .into_owned(),
                ),
                proof_kind: ValidationProofKind::Focused,
                evidence: ValidationEvidence {
                    candidate_id: "typed-candidate".to_string(),
                    implementation_identity,
                    source_evidence_epoch: Some(workspace_epoch),
                    normalized_invocation: "cargo test typed".to_string(),
                    coverage_identity,
                    start_epoch: workspace_epoch,
                    end_epoch: Some(workspace_epoch),
                    covered_scopes: Vec::new(),
                    covered_manifest: vec![WorkspaceManifestEntry {
                        path: "codex-rs/core/src/task_evidence.rs".to_string(),
                        content_hash: Some("typed-content".to_string()),
                        existed: true,
                    }],
                    execution_snapshot: None,
                    covered_contracts: vec!["typed-child-final-proof".to_string()],
                    manifest_hash: "typed-manifest".to_string(),
                    repository_wide: false,
                    cwd: Some(repository),
                    environment_hash: Some("typed-environment".to_string()),
                    toolchain: Some("typed-toolchain".to_string()),
                    features_configuration_identity: "typed-configuration".to_string(),
                    covered_input_manifest_hash: "typed-inputs".to_string(),
                    dependency_manifest_hash: "typed-dependencies".to_string(),
                    successful_result: Some(true),
                    retained_output_digest: "typed-output-digest".to_string(),
                    retained_output_ref: Some("artifact://typed-output".to_string()),
                    output_summary: Some("typed validation passed".to_string()),
                    validation_result: Some(
                        serde_json::to_value(validation_result)
                            .expect("validation result serializes"),
                    ),
                    lease_expires_at: None,
                    shared_from_call_id: None,
                    stale_reason: None,
                },
                status: ValidationCallStatus::Succeeded,
                recorded_at: now,
            }],
            workspace_status: WorkspaceTaskStatus {
                epoch: workspace_epoch,
                ..WorkspaceTaskStatus::default()
            },
            isolation_handoff: None,
            integration_handoffs: Vec::new(),
            observations: Vec::new(),
        }
    }

    #[test]
    fn typed_child_validation_receipt_collector_reuses_scoped_proof_after_unrelated_epoch() {
        let mut task = completed_typed_task_with_structured_validation();
        let proofs = authoritative_typed_validation_proofs(&task);
        assert_eq!(proofs.len(), 1);
        assert_eq!(proofs[0].call_id, "typed-validation");
        assert_eq!(proofs[0].retained_output_ref, "artifact://typed-output");

        task.workspace_status.epoch = task.workspace_status.epoch.saturating_add(1);
        assert_eq!(authoritative_typed_validation_proofs(&task).len(), 1);
        task.validation_calls[0].evidence.end_epoch =
            Some(task.workspace_status.epoch.saturating_add(1));
        assert!(authoritative_typed_validation_proofs(&task).is_empty());
    }

    #[tokio::test]
    async fn reviewer_config_disables_recursive_completion_review() {
        let codex_home = tempdir().expect("tempdir should succeed");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("config should build");
        config
            .features
            .enable(Feature::TaskCompletionReviewer)
            .expect("feature is mutable");

        disable_reviewer_features(&mut config).expect("reviewer features should be mutable");

        assert!(!config.features.enabled(Feature::TaskCompletionReviewer));
    }

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

    #[test]
    fn stability_gate_only_blocks_unstable_or_unavailable_evidence() {
        let current = CompletionReviewTurnEvidence {
            exact_diff: Some("diff-a".to_string()),
            mutation_revision: 3,
            validation_freshness: ValidationFreshnessStatus::PassedAfterLastMutation,
            last_successful_validation_revision: Some(3),
        };
        assert!(review_stability_blocker_reasons(&dossier()).is_empty());

        let mut stale = current;
        stale.last_successful_validation_revision = Some(2);
        assert!(review_stability_blocker_reasons(&dossier()).is_empty());
        assert_eq!(
            review_admission_decision_for_source(
                &SessionSource::Cli,
                &dossier(),
                &ReviewObligationMode::Supplemental,
                &stale,
            ),
            ReviewAdmissionDecision::Admit,
            "stale deterministic proof must admit supplemental review"
        );

        let mut blocked = dossier();
        blocked.evidence_gate = TaskCompletionGate {
            status: TaskCompletionStatus::Blocked,
            reasons: vec!["plan dependency is blocked".to_string()],
            evidence_path: None,
        };
        blocked.typed_quiescent = false;
        blocked.default_children_quiescent = false;
        blocked
            .authoritative_input_errors
            .push("typed evidence unavailable".to_string());
        let reasons = review_stability_blocker_reasons(&blocked);
        assert!(
            !reasons
                .iter()
                .any(|reason| reason.contains("plan dependency"))
        );
        assert!(reasons.iter().any(|reason| reason.contains("typed task")));
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("default child"))
        );
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("typed evidence"))
        );

        let mut non_mutating = dossier();
        non_mutating.has_task_attributed_mutations = false;
        assert!(
            review_stability_blocker_reasons(&non_mutating).is_empty(),
            "a zero-command candidate must not invent a validation requirement"
        );

        let mut unavailable = dossier();
        unavailable.source_capture_failed = true;
        unavailable.sources[0].availability = UserSourceAvailability::Unavailable;
        let reasons = review_stability_blocker_reasons(&unavailable);
        assert!(reasons.iter().any(|reason| reason.contains("captured")));
        assert!(reasons.iter().any(|reason| reason.contains("unavailable")));
    }

    #[test]
    fn mandatory_obligation_precedes_fresh_low_risk_admission_skip() {
        let mut review_dossier = dossier();
        review_dossier
            .review_lens_selection_facts
            .task_mutation_paths = vec!["src/lib.rs".to_string()];
        let fresh = CompletionReviewTurnEvidence {
            exact_diff: Some("diff-a".to_string()),
            mutation_revision: 3,
            validation_freshness: ValidationFreshnessStatus::PassedAfterLastMutation,
            last_successful_validation_revision: Some(3),
        };

        assert_eq!(
            review_admission_decision_for_source(
                &SessionSource::Cli,
                &review_dossier,
                &ReviewObligationMode::Supplemental,
                &fresh,
            ),
            ReviewAdmissionDecision::SkipFreshLowRisk,
            "supplemental review retains the risk-based skip"
        );
        assert_eq!(
            review_admission_decision_for_source(
                &SessionSource::Cli,
                &review_dossier,
                &ReviewObligationMode::Mandatory {
                    requirement_ids: vec!["requirement-1".to_string()],
                    obligation_hash: "mandatory-obligation".to_string(),
                },
                &fresh,
            ),
            ReviewAdmissionDecision::Admit,
            "an explicit mandatory obligation cannot be skipped as fresh and low-risk"
        );
    }

    #[test]
    fn completion_attempt_identity_covers_every_reuse_identity_class() {
        let base_dossier = dossier();
        let lenses = selected_lenses(&base_dossier);
        let base_evidence = CompletionReviewTurnEvidence {
            exact_diff: Some("diff-a".to_string()),
            mutation_revision: 3,
            validation_freshness: ValidationFreshnessStatus::PassedAfterLastMutation,
            last_successful_validation_revision: Some(3),
        };
        let base_obligation = ReviewObligationMode::Mandatory {
            requirement_ids: vec!["requirement-1".to_string()],
            obligation_hash: "obligation-a".to_string(),
        };
        let base_contract = ReviewerExecutionContract {
            contract_version: REVIEWER_EXECUTION_CONTRACT_VERSION,
            reviewer_model: "review-model-a".to_string(),
            reviewer_provider: "provider-a".to_string(),
            reasoning_configuration: "effort=High;summary=Detailed".to_string(),
            reviewer_prompt_hash: stable_hash(&REVIEWER_BASE_INSTRUCTIONS),
            output_schema_version: REVIEW_OUTPUT_SCHEMA_VERSION,
            tool_capability_hash: "tools-a".to_string(),
            source_classification_contract_version:
                crate::task_evidence::SOURCE_CLASSIFICATION_CONTRACT_VERSION,
            relationship_resolver_contract_version:
                crate::task_evidence::RELATIONSHIP_RESOLVER_CONTRACT_VERSION,
            review_feature_hash: "features-a".to_string(),
        };
        let identity = |dossier: &CompletionReviewDossier,
                        evidence: &CompletionReviewTurnEvidence,
                        obligation: &ReviewObligationMode,
                        contract: &ReviewerExecutionContract,
                        attempt_kind: CompletionReviewAttemptKind| {
            let bounded = bounded_review_dossier_json(
                dossier,
                attempt_kind == CompletionReviewAttemptKind::Rereview,
                &lenses,
                obligation,
                evidence,
            )
            .expect("bounded dossier");
            review_attempt_identity(attempt_kind, dossier, &bounded, contract).value
        };
        let base = identity(
            &base_dossier,
            &base_evidence,
            &base_obligation,
            &base_contract,
            CompletionReviewAttemptKind::Initial,
        );

        let mut changed_dossier = base_dossier.clone();
        changed_dossier.implementation_identity_hash = "implementation-b".to_string();
        assert_ne!(
            base,
            identity(
                &changed_dossier,
                &base_evidence,
                &base_obligation,
                &base_contract,
                CompletionReviewAttemptKind::Initial,
            )
        );
        changed_dossier = base_dossier.clone();
        changed_dossier.requirement_manifest_hash = "manifest-b".to_string();
        assert_ne!(
            base,
            identity(
                &changed_dossier,
                &base_evidence,
                &base_obligation,
                &base_contract,
                CompletionReviewAttemptKind::Initial,
            )
        );
        changed_dossier = base_dossier.clone();
        changed_dossier.user_source_ledger_hash = "source-ledger-b".to_string();
        assert_ne!(
            base,
            identity(
                &changed_dossier,
                &base_evidence,
                &base_obligation,
                &base_contract,
                CompletionReviewAttemptKind::Initial,
            )
        );

        let mut changed_evidence = base_evidence.clone();
        changed_evidence.validation_freshness = ValidationFreshnessStatus::FailedAfterLastMutation;
        assert_ne!(
            base,
            identity(
                &base_dossier,
                &changed_evidence,
                &base_obligation,
                &base_contract,
                CompletionReviewAttemptKind::Initial,
            )
        );
        changed_evidence = base_evidence.clone();
        changed_evidence.exact_diff = Some("diff-b".to_string());
        assert_ne!(
            base,
            identity(
                &base_dossier,
                &changed_evidence,
                &base_obligation,
                &base_contract,
                CompletionReviewAttemptKind::Initial,
            )
        );

        assert_ne!(
            base,
            identity(
                &base_dossier,
                &base_evidence,
                &ReviewObligationMode::Supplemental,
                &base_contract,
                CompletionReviewAttemptKind::Initial,
            )
        );
        assert_ne!(
            base,
            identity(
                &base_dossier,
                &base_evidence,
                &base_obligation,
                &base_contract,
                CompletionReviewAttemptKind::Rereview,
            )
        );

        for mutate in [
            "model",
            "provider",
            "reasoning",
            "prompt",
            "schema",
            "tools",
            "features",
        ] {
            let mut contract = base_contract.clone();
            match mutate {
                "model" => contract.reviewer_model = "review-model-b".to_string(),
                "provider" => contract.reviewer_provider = "provider-b".to_string(),
                "reasoning" => contract.reasoning_configuration = "effort=Low".to_string(),
                "prompt" => contract.reviewer_prompt_hash = "prompt-b".to_string(),
                "schema" => contract.output_schema_version = "review-output-b",
                "tools" => contract.tool_capability_hash = "tools-b".to_string(),
                "features" => contract.review_feature_hash = "features-b".to_string(),
                _ => unreachable!(),
            }
            assert_ne!(
                base,
                identity(
                    &base_dossier,
                    &base_evidence,
                    &base_obligation,
                    &contract,
                    CompletionReviewAttemptKind::Initial,
                ),
                "{mutate} must invalidate completion-review reuse"
            );
        }
    }

    fn selected_lenses(dossier: &CompletionReviewDossier) -> SelectedReviewLenses {
        select_review_lenses(
            &build_review_lens_selection_input(dossier).expect("valid selection input"),
        )
    }

    #[test]
    fn review_obligation_is_mandatory_only_for_an_active_exact_semantic_cue() {
        let mut review_dossier = dossier();
        assert_eq!(
            resolve_review_obligation(&review_dossier),
            ReviewObligationResolution::Resolved(ReviewObligationMode::Supplemental)
        );

        let source = review_dossier.sources[0].clone();
        let requirement = review_dossier.requirements[0].clone();
        review_dossier
            .source_classification_cache
            .get_mut(&source_classification_cache_key(&source))
            .expect("source classification")
            .local_semantic_cues = vec![LocalSemanticCue {
            kind: LocalSemanticCueKind::MandatoryCompletionReview,
            source_span: Some(requirement.source_span.clone()),
        }];
        let resolved = resolve_review_obligation(&review_dossier);
        let ReviewObligationResolution::Resolved(ReviewObligationMode::Mandatory {
            requirement_ids,
            obligation_hash,
        }) = resolved
        else {
            panic!("exact active cue must require review: {resolved:?}");
        };
        assert_eq!(requirement_ids, vec![requirement.requirement_id]);
        assert!(!obligation_hash.is_empty());

        review_dossier.requirements[0].status = RequirementStatus::Superseded;
        assert_eq!(
            resolve_review_obligation(&review_dossier),
            ReviewObligationResolution::Resolved(ReviewObligationMode::Supplemental)
        );
    }

    #[test]
    fn review_obligation_never_guesses_when_source_classification_is_missing() {
        let mut review_dossier = dossier();
        review_dossier.source_classification_cache.clear();
        assert_eq!(
            resolve_review_obligation(&review_dossier),
            ReviewObligationResolution::NeedsObligationMaterialization
        );
    }

    #[test]
    fn disabled_reviewer_preserves_an_explicit_mandatory_obligation() {
        let mut review_dossier = dossier();
        assert_eq!(
            resolve_disabled_review_requirement(&review_dossier),
            ReviewObligationMode::Disabled
        );

        let source = review_dossier.sources[0].clone();
        let requirement = review_dossier.requirements[0].clone();
        review_dossier
            .source_classification_cache
            .get_mut(&source_classification_cache_key(&source))
            .expect("source classification")
            .local_semantic_cues = vec![LocalSemanticCue {
            kind: LocalSemanticCueKind::MandatoryCompletionReview,
            source_span: Some(requirement.source_span.clone()),
        }];
        let resolved = resolve_disabled_review_requirement(&review_dossier);
        let ReviewObligationMode::Mandatory {
            requirement_ids, ..
        } = resolved
        else {
            panic!("disabled reviewer must not erase a mandatory obligation: {resolved:?}");
        };
        assert_eq!(requirement_ids, vec![requirement.requirement_id]);

        review_dossier.source_classification_cache.clear();
        assert_eq!(
            resolve_disabled_review_requirement(&review_dossier),
            ReviewObligationMode::Disabled,
            "missing source classification must not invent a mandatory obligation"
        );
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
    fn evidence_only_correction_is_never_created_without_an_actionable_finding() {
        let mut dossier = dossier();
        assert!(build_repair_item(&dossier, &[]).is_none());

        dossier.locally_obtainable_proof_routes =
            vec!["run the focused generated-artifact proof and record its receipt".to_string()];
        assert!(build_repair_item(&dossier, &[]).is_none());
    }

    #[test]
    fn oversized_reviewer_evidence_prevents_an_unbounded_correction() {
        let mut dossier = dossier();
        dossier.reviewer_visible_evidence =
            json!({"oversized": "x".repeat(MAX_RENDERED_REQUEST_TOKENS * 8)});
        let finding = CompletionReviewFindingReceipt {
            finding_id: "review-2/F1".to_string(),
            requirement_ids: vec!["requirement-1".to_string()],
            lens: REVIEW_LENSES[0].to_string(),
            contract_surface: "completion coordinator".to_string(),
            severity: "high".to_string(),
            evidence: "the defect remains".to_string(),
            smallest_correction: "finish the missing behavior".to_string(),
            proof_route: "run the focused regression".to_string(),
        };

        assert!(build_repair_item(&dossier, std::slice::from_ref(&finding)).is_none());
    }

    #[test]
    fn reviewer_requests_only_expose_dossier_bound_evidence() {
        let mut dossier = dossier();
        dossier.locally_obtainable_proof_routes = vec!["run focused proof".to_string()];
        dossier.reviewer_visible_evidence = json!({
            "proofReceipts": [{"command": "cargo test focused_case", "passed": true}],
            "unboundedInternalDetail": "must not reach the reviewer",
        });

        let selected = selected_lenses(&dossier);
        let review: Value = serde_json::from_str(&review_dossier_json(&dossier, false, &selected))
            .expect("review JSON");
        assert!(review.get("evidence_summary").is_none());
        assert_eq!(
            review["validation"]["focused_receipts"],
            dossier.reviewer_visible_evidence["proofReceipts"]
        );
        assert!(review.get("reviewer_visible_evidence").is_none());
        assert!(!review.to_string().contains("unboundedInternalDetail"));

        let (_, correction) = build_repair_item(&dossier, &[original_finding()])
            .expect("actionable correction payload");
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
