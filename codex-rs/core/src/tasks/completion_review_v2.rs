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
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

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
use crate::task_evidence::ManifestGapInput;
use crate::task_evidence::RecordedReviewAttempt;
use crate::task_evidence::RequirementRecord;
use crate::task_evidence::RequirementStatus;
use crate::task_evidence::SourceMapping;
use crate::task_evidence::SourceSpan;
use crate::task_evidence::TaskEvidenceLedger;
use crate::task_evidence::UserSourceAvailability;
use crate::task_evidence::UserSourceKind;
use crate::task_evidence::UserSourceRecord;
use crate::task_evidence::sha256_file;

const REVIEW_DEADLINE: Duration = Duration::from_secs(90);
const REVIEW_CLEANUP_DEADLINE: Duration = Duration::from_secs(5);
const MAX_RENDERED_REQUEST_TOKENS: usize = 8_999;
const MAX_REVIEW_OUTPUT_TOKENS: usize = 6_000;
const MAX_REVIEW_FINDINGS: usize = 32;
const AUTHORITATIVE_MUTATION_EVIDENCE_LIMIT: usize = 100;

const SOURCE_CLASSIFICATION_MARKER: &str = "KD4_SOURCE_CLASSIFICATION_REQUEST_V1";
const REVIEW_REQUEST_MARKER: &str = "KD4_COMPLETION_REVIEW_REQUEST_V2";

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceReviewResultKind {
    RequirementBearing,
    ManifestGap,
    NonRequirement,
    SupersededContext,
    UnavailableOrTruncated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceReviewResult {
    source_id: String,
    result: SourceReviewResultKind,
    requirement_ids: Vec<String>,
    omitted_source_spans: Vec<WireSpan>,
    reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RequirementReviewResult {
    requirement_id: String,
    status: WireRequirementStatus,
    satisfied: bool,
    evidence: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LensStatus {
    Checked,
    Inapplicable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LensReviewResult {
    lens: String,
    status: LensStatus,
    surfaces: Vec<String>,
    evidence: String,
    reason: String,
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
struct CompletionReviewOutput {
    clean: bool,
    sources: Vec<SourceReviewResult>,
    requirements: Vec<RequirementReviewResult>,
    lenses: Vec<LensReviewResult>,
    findings: Vec<ReviewFinding>,
    original_finding_dispositions: Vec<ReviewDisposition>,
}

#[derive(Debug)]
enum ReviewerPayload {
    Classification(SourceClassificationOutput),
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
    InitialReview,
    Rereview,
}

#[derive(Debug)]
struct ValidatedReview {
    review_clean: bool,
    manifest_gaps: Vec<ManifestGapInput>,
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

fn completion_review_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "clean": { "type": "boolean" },
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
                                "manifest_gap",
                                "non_requirement",
                                "superseded_context",
                                "unavailable_or_truncated"
                            ]
                        },
                        "requirement_ids": { "type": "array", "items": { "type": "string" } },
                        "omitted_source_spans": { "type": "array", "items": wire_span_schema() },
                        "reason": { "type": "string" }
                    },
                    "required": [
                        "source_id",
                        "result",
                        "requirement_ids",
                        "omitted_source_spans",
                        "reason"
                    ]
                }
            },
            "requirements": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "requirement_id": { "type": "string" },
                        "status": {
                            "type": "string",
                            "enum": ["active", "superseded", "withdrawn"]
                        },
                        "satisfied": { "type": "boolean" },
                        "evidence": { "type": "string" }
                    },
                    "required": ["requirement_id", "status", "satisfied", "evidence"]
                }
            },
            "lenses": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "lens": { "type": "string", "enum": REVIEW_LENSES },
                        "status": { "type": "string", "enum": ["checked", "inapplicable"] },
                        "surfaces": { "type": "array", "items": { "type": "string" } },
                        "evidence": { "type": "string" },
                        "reason": { "type": "string" }
                    },
                    "required": ["lens", "status", "surfaces", "evidence", "reason"]
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
                        "requirement_ids": { "type": "array", "minItems": 1, "items": { "type": "string" } },
                        "lens": { "type": "string", "enum": REVIEW_LENSES },
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
            "original_finding_dispositions": {
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
            "clean",
            "sources",
            "requirements",
            "lenses",
            "findings",
            "original_finding_dispositions"
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
    parent_cancellation: &CancellationToken,
) -> CodexResult<ReviewerExecution> {
    let review_cancellation = CancellationToken::new();
    let mut run = Box::pin(run_reviewer_once(
        Arc::clone(sess),
        Arc::clone(turn_context),
        inputs,
        kind,
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
        ReviewerRequestKind::InitialReview | ReviewerRequestKind::Rereview => {
            completion_review_output_schema()
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
) -> Result<Vec<UserInput>, ReviewFailureCategory> {
    let request = match kind {
        ReviewerRequestKind::Classification => format!(
            "{SOURCE_CLASSIFICATION_MARKER}\n\nClassify every supplied immutable user source exactly once. Split each source into real requirements, non-requirement context, superseded context, or unavailable/truncated content. Requirements must use exact immutable spans. Text spans are UTF-8 byte offsets with 0 <= start < end <= source length; set reference and subreference to empty strings. Image and attachment spans use start=end=0 and copy the supplied source exact_material value into reference; that value is a bounded review reference, while an attached image input supplies image bytes. Use subreference only for a concrete region/range. Active and withdrawn requirements use empty superseded_by fields and an empty text span sentinel (kind=text,start=0,end=0,empty strings). A superseded requirement must point to another requirement span in this same response. Do not infer requirements from model summaries, plans, or tests.\n\n<source_ledger>\n{}\n</source_ledger>",
            classification_dossier_json(dossier)
        ),
        ReviewerRequestKind::InitialReview => format!(
            "{REVIEW_REQUEST_MARKER}\n\nIndependently review this exact candidate. Return every supplied source ID, requirement ID, and named lens exactly once. For RequirementBearing return exactly all manifest requirement IDs whose provenance is that source. Use ManifestGap only when exact immutable source material contains a real omitted requirement, and locate it with the supplied span format. A finding must be contract-relevant and reference existing requirement IDs. Return no original-finding dispositions for an initial review.\n\n<completion_dossier>\n{}\n</completion_dossier>",
            review_dossier_json(dossier, false)
        ),
        ReviewerRequestKind::Rereview => format!(
            "{REVIEW_REQUEST_MARKER}\n\nattempt_kind=rereview\nIndependently rereview the original active requirements, complete original finding set, correction or rebuttal delta represented by the new candidate, changed tests/snapshots/fixtures/generators, and fresh proof receipts. Return every source, requirement, and lens exactly once. Disposition every original finding ID exactly once and check both that it was fixed or rebutted and that the correction caused no regression. New defects use local finding ordinals; do not invent durable IDs.\n\n<completion_dossier>\n{}\n</completion_dossier>",
            review_dossier_json(dossier, true)
        ),
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

fn review_dossier_json(dossier: &CompletionReviewDossier, rereview: bool) -> String {
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
        "review_lenses": REVIEW_LENSES,
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

fn validate_review_output(
    dossier: &CompletionReviewDossier,
    output: CompletionReviewOutput,
    rereview: bool,
) -> Option<ValidatedReview> {
    let expected_sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let returned_source_ids = output
        .sources
        .iter()
        .map(|source| source.source_id.as_str())
        .collect::<BTreeSet<_>>();
    if returned_source_ids.len() != output.sources.len()
        || returned_source_ids != expected_sources.keys().copied().collect()
    {
        return None;
    }

    let expected_requirements = dossier
        .requirements
        .iter()
        .map(|requirement| (requirement.requirement_id.as_str(), requirement))
        .collect::<BTreeMap<_, _>>();
    let returned_requirement_ids = output
        .requirements
        .iter()
        .map(|requirement| requirement.requirement_id.as_str())
        .collect::<BTreeSet<_>>();
    if returned_requirement_ids.len() != output.requirements.len()
        || returned_requirement_ids != expected_requirements.keys().copied().collect()
    {
        return None;
    }

    let mut manifest_gaps = Vec::new();
    let mut unavailable_source = false;
    for result in &output.sources {
        let source = expected_sources.get(result.source_id.as_str())?;
        let mut expected_ids = dossier
            .requirements
            .iter()
            .filter(|requirement| requirement.source_id == result.source_id)
            .map(|requirement| requirement.requirement_id.clone())
            .collect::<Vec<_>>();
        expected_ids.sort();
        let mut returned_ids = result.requirement_ids.clone();
        returned_ids.sort();
        if returned_ids != expected_ids || returned_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return None;
        }
        let mapping = dossier.source_mappings.get(&result.source_id)?;
        let shape_valid = match result.result {
            SourceReviewResultKind::RequirementBearing => {
                matches!(mapping, SourceMapping::RequirementBearing { .. })
                    && !result.requirement_ids.is_empty()
                    && result.omitted_source_spans.is_empty()
                    && result.reason.trim().is_empty()
            }
            SourceReviewResultKind::ManifestGap => {
                !result.omitted_source_spans.is_empty() && result.reason.trim().is_empty()
            }
            SourceReviewResultKind::NonRequirement => {
                matches!(mapping, SourceMapping::NonRequirement { .. })
                    && result.requirement_ids.is_empty()
                    && result.omitted_source_spans.is_empty()
                    && !result.reason.trim().is_empty()
            }
            SourceReviewResultKind::SupersededContext => {
                matches!(mapping, SourceMapping::SupersededContext { .. })
                    && result.requirement_ids.is_empty()
                    && result.omitted_source_spans.is_empty()
                    && !result.reason.trim().is_empty()
            }
            SourceReviewResultKind::UnavailableOrTruncated => {
                matches!(mapping, SourceMapping::UnavailableOrTruncated)
                    && result.requirement_ids.is_empty()
                    && result.omitted_source_spans.is_empty()
            }
        };
        if !shape_valid {
            return None;
        }
        if result.result == SourceReviewResultKind::ManifestGap {
            let omitted_spans = result
                .omitted_source_spans
                .iter()
                .map(|span| wire_span_to_source_span(source, span))
                .collect::<Option<Vec<_>>>()?;
            manifest_gaps.push(ManifestGapInput {
                source_id: result.source_id.clone(),
                omitted_spans,
            });
        }
        unavailable_source |= result.result == SourceReviewResultKind::UnavailableOrTruncated;
    }

    let mut unsatisfied_active_requirement_ids = BTreeSet::new();
    for result in &output.requirements {
        let expected = expected_requirements.get(result.requirement_id.as_str())?;
        if wire_requirement_status(result.status) != expected.status
            || result.evidence.trim().is_empty()
        {
            return None;
        }
        if expected.status == RequirementStatus::Active && !result.satisfied {
            unsatisfied_active_requirement_ids.insert(result.requirement_id.as_str());
        }
    }

    let returned_lenses = output
        .lenses
        .iter()
        .map(|lens| lens.lens.as_str())
        .collect::<BTreeSet<_>>();
    if returned_lenses.len() != output.lenses.len()
        || returned_lenses != REVIEW_LENSES.into_iter().collect()
        || output.lenses.iter().any(|lens| match lens.status {
            LensStatus::Checked => {
                lens.surfaces.is_empty()
                    || lens
                        .surfaces
                        .iter()
                        .any(|surface| surface.trim().is_empty())
                    || lens.evidence.trim().is_empty()
                    || !lens.reason.trim().is_empty()
            }
            LensStatus::Inapplicable => {
                !lens.surfaces.is_empty()
                    || !lens.evidence.trim().is_empty()
                    || lens.reason.trim().is_empty()
            }
        })
    {
        return None;
    }
    let checked_lenses = output
        .lenses
        .iter()
        .filter(|lens| lens.status == LensStatus::Checked)
        .map(|lens| lens.lens.as_str())
        .collect::<BTreeSet<_>>();

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
    let findings = output
        .findings
        .iter()
        .map(|finding| {
            let referenced_ids = finding
                .requirement_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            if finding.requirement_ids.is_empty()
                || referenced_ids.len() != finding.requirement_ids.len()
                || !referenced_ids.iter().any(|requirement_id| {
                    expected_requirements
                        .get(requirement_id)
                        .is_some_and(|requirement| requirement.status == RequirementStatus::Active)
                })
                || finding.requirement_ids.iter().any(|requirement_id| {
                    !expected_requirements.contains_key(requirement_id.as_str())
                })
                || !checked_lenses.contains(finding.lens.as_str())
                || finding.contract_surface.trim().is_empty()
                || finding.concrete_evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.focused_proof_route.trim().is_empty()
            {
                return None;
            }
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
    let finding_requirement_ids = findings
        .iter()
        .flat_map(|finding| finding.requirement_ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    if !unsatisfied_active_requirement_ids.is_subset(&finding_requirement_ids) {
        return None;
    }

    let expected_original_findings = dossier
        .original_findings
        .iter()
        .map(|finding| finding.finding_id.as_str())
        .collect::<BTreeSet<_>>();
    let returned_dispositions = output
        .original_finding_dispositions
        .iter()
        .map(|disposition| disposition.finding_id.as_str())
        .collect::<BTreeSet<_>>();
    if (!rereview && !output.original_finding_dispositions.is_empty())
        || (rereview
            && (returned_dispositions.len() != output.original_finding_dispositions.len()
                || returned_dispositions != expected_original_findings))
        || output
            .original_finding_dispositions
            .iter()
            .any(|disposition| disposition.evidence.trim().is_empty())
    {
        return None;
    }
    let dispositions = output
        .original_finding_dispositions
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
    let original_findings_clean = !rereview
        || output
            .original_finding_dispositions
            .iter()
            .all(|disposition| {
                matches!(
                    disposition.disposition,
                    FindingDisposition::Resolved | FindingDisposition::RebuttalAccepted
                )
            });
    if rereview && !manifest_gaps.is_empty() && !original_findings_clean {
        return None;
    }
    let review_clean = manifest_gaps.is_empty()
        && !unavailable_source
        && unsatisfied_active_requirement_ids.is_empty()
        && findings.is_empty()
        && original_findings_clean;
    if output.clean != review_clean {
        return None;
    }
    Some(ValidatedReview {
        review_clean,
        manifest_gaps,
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
        let inputs =
            match build_reviewer_inputs(&dossier, ReviewerRequestKind::Classification).await {
                Ok(inputs) => inputs,
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
        let execution = match sess.try_acquire_completion_review_slot() {
            Some(_permit) => {
                run_reviewer_with_deadline(
                    sess,
                    turn_context,
                    inputs,
                    ReviewerRequestKind::Classification,
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
                CompletionReviewAttemptKind::InitialReview,
                None,
                ReviewFailureCategory::SourceDrift,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::SourceDrift));
        }
        let Some(ReviewerPayload::Classification(output)) = execution.payload else {
            let failure = execution
                .failures
                .first()
                .copied()
                .unwrap_or(ReviewFailureCategory::MalformedOutput);
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
        };
        let Some(classifications) = validate_classification(&dossier, output) else {
            persist_review_failure(
                sess,
                &dossier,
                CompletionReviewAttemptKind::InitialReview,
                None,
                ReviewFailureCategory::MalformedOutput,
            )
            .await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(ReviewFailureCategory::MalformedOutput));
        };
        match sess
            .services
            .task_evidence
            .apply_source_classification(&dossier, classifications)
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
    run_contract_review(
        sess,
        turn_context,
        cancellation_token,
        candidate_completion,
        state,
        dossier,
        kind,
        false,
    )
    .await
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
) -> CodexResult<CompletionReviewCoordinatorOutcome> {
    let attempt_kind = match kind {
        ReviewerRequestKind::InitialReview => CompletionReviewAttemptKind::InitialReview,
        ReviewerRequestKind::Rereview => CompletionReviewAttemptKind::Rereview,
        ReviewerRequestKind::Classification => unreachable!(),
    };
    let parent_review_id = match kind {
        ReviewerRequestKind::InitialReview => dossier.cycle_parent_review_id.clone(),
        ReviewerRequestKind::Rereview => dossier.initial_review_id.clone(),
        ReviewerRequestKind::Classification => unreachable!(),
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
    let inputs = match build_reviewer_inputs(&dossier, kind).await {
        Ok(inputs) => inputs,
        Err(failure) => {
            persist_review_failure(sess, &dossier, attempt_kind, parent_review_id, failure).await;
            state.phase = TurnReviewPhase::Terminal;
            return Ok(partial_outcome(failure));
        }
    };
    let execution = match sess.try_acquire_completion_review_slot() {
        Some(_permit) => {
            run_reviewer_with_deadline(sess, turn_context, inputs, kind, cancellation_token).await?
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
    if fresh_dossier.implementation_identity_hash != dossier.implementation_identity_hash
        || fresh_dossier.dossier_snapshot_id != dossier.dossier_snapshot_id
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
        match persist_validated_attempt(
            sess,
            &dossier,
            attempt_kind,
            parent_review_id,
            validated,
            None,
            None,
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
    run_contract_review(
        sess,
        turn_context,
        cancellation_token,
        candidate_completion,
        state,
        after_correction,
        ReviewerRequestKind::Rereview,
        false,
    )
    .await
}

async fn persist_validated_attempt(
    sess: &Session,
    dossier: &CompletionReviewDossier,
    attempt_kind: CompletionReviewAttemptKind,
    parent_review_id: Option<String>,
    validated: ValidatedReview,
    repair_instruction: Option<String>,
    terminal_outcome: Option<&str>,
) -> Option<RecordedReviewAttempt> {
    match sess
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
                findings: validated.findings,
                dispositions: validated.dispositions,
                manifest_gaps: validated.manifest_gaps,
                repair_instruction,
                repair_instruction_hash: None,
                infrastructure_outcome: "ok".to_string(),
                review_clean: validated.review_clean,
                terminal_outcome: terminal_outcome.map(str::to_string),
            },
        )
        .await
    {
        AtomicReviewTransition::Persisted(recorded) => Some(recorded),
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
    let payload = serde_json::to_string_pretty(&json!({
        "contract": "KD4_COMPLETION_CORRECTION_V2",
        "root_task_id": dossier.root_task_id,
        "completion_epoch": dossier.completion_epoch,
        "manifest_revision": dossier.manifest_revision,
        "implementation_identity": dossier.implementation_identity_hash,
        "reviewed_dossier_snapshot_id": dossier.dossier_snapshot_id,
        "active_requirements": active_requirements,
        "complete_finding_set": findings,
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
        }
    }

    fn checked_lenses() -> Vec<LensReviewResult> {
        REVIEW_LENSES
            .iter()
            .map(|lens| LensReviewResult {
                lens: (*lens).to_string(),
                status: LensStatus::Checked,
                surfaces: vec!["bounded owner".to_string()],
                evidence: "owner and one-hop evidence checked".to_string(),
                reason: String::new(),
            })
            .collect()
    }

    fn clean_output() -> CompletionReviewOutput {
        CompletionReviewOutput {
            clean: true,
            sources: vec![SourceReviewResult {
                source_id: "source-1".to_string(),
                result: SourceReviewResultKind::RequirementBearing,
                requirement_ids: vec!["requirement-1".to_string()],
                omitted_source_spans: Vec::new(),
                reason: String::new(),
            }],
            requirements: vec![RequirementReviewResult {
                requirement_id: "requirement-1".to_string(),
                status: WireRequirementStatus::Active,
                satisfied: true,
                evidence: "implemented and focused proof passed".to_string(),
            }],
            lenses: checked_lenses(),
            findings: Vec::new(),
            original_finding_dispositions: Vec::new(),
        }
    }

    fn valid_finding() -> ReviewFinding {
        ReviewFinding {
            finding_local_ordinal: 1,
            requirement_ids: vec!["requirement-1".to_string()],
            lens: REVIEW_LENSES[0].to_string(),
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

    #[test]
    fn review_contract_accepts_clean_and_precise_manifest_gap_results() {
        let dossier = dossier();
        let clean =
            validate_review_output(&dossier, clean_output(), false).expect("clean review output");
        assert!(clean.review_clean);
        assert!(clean.manifest_gaps.is_empty());

        let mut gap_output = clean_output();
        gap_output.clean = false;
        gap_output.sources[0].result = SourceReviewResultKind::ManifestGap;
        gap_output.sources[0].omitted_source_spans = vec![text_span(15, 24)];
        let gap =
            validate_review_output(&dossier, gap_output, false).expect("precise manifest gap");
        assert!(!gap.review_clean);
        assert_eq!(gap.manifest_gaps.len(), 1);
        assert_eq!(
            gap.manifest_gaps[0].omitted_spans,
            vec![SourceSpan::Text { start: 15, end: 24 }]
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum InvalidReviewCase {
        MissingSource,
        DuplicateSource,
        SourceRequirementMismatch,
        MissingRequirement,
        DuplicateRequirement,
        MissingLens,
        DuplicateLens,
        UnknownLens,
        InapplicableWithoutReason,
        CheckedWithoutEvidence,
        NonContiguousFinding,
        UnknownFindingRequirement,
        BlankFindingEvidence,
        CleanFlagMismatch,
        DispositionOnInitial,
    }

    #[test]
    fn host_rejects_incomplete_duplicate_unknown_and_inconsistent_review_output() {
        let dossier = dossier();
        let cases = [
            InvalidReviewCase::MissingSource,
            InvalidReviewCase::DuplicateSource,
            InvalidReviewCase::SourceRequirementMismatch,
            InvalidReviewCase::MissingRequirement,
            InvalidReviewCase::DuplicateRequirement,
            InvalidReviewCase::MissingLens,
            InvalidReviewCase::DuplicateLens,
            InvalidReviewCase::UnknownLens,
            InvalidReviewCase::InapplicableWithoutReason,
            InvalidReviewCase::CheckedWithoutEvidence,
            InvalidReviewCase::NonContiguousFinding,
            InvalidReviewCase::UnknownFindingRequirement,
            InvalidReviewCase::BlankFindingEvidence,
            InvalidReviewCase::CleanFlagMismatch,
            InvalidReviewCase::DispositionOnInitial,
        ];
        for case in cases {
            let mut output = clean_output();
            match case {
                InvalidReviewCase::MissingSource => output.sources.clear(),
                InvalidReviewCase::DuplicateSource => {
                    output.sources.push(output.sources[0].clone());
                }
                InvalidReviewCase::SourceRequirementMismatch => {
                    output.sources[0].requirement_ids.clear();
                }
                InvalidReviewCase::MissingRequirement => output.requirements.clear(),
                InvalidReviewCase::DuplicateRequirement => {
                    output.requirements.push(output.requirements[0].clone());
                }
                InvalidReviewCase::MissingLens => {
                    output.lenses.pop();
                }
                InvalidReviewCase::DuplicateLens => {
                    output.lenses.push(output.lenses[0].clone());
                }
                InvalidReviewCase::UnknownLens => {
                    output.lenses[0].lens = "unknown_lens".to_string();
                }
                InvalidReviewCase::InapplicableWithoutReason => {
                    output.lenses[0].status = LensStatus::Inapplicable;
                    output.lenses[0].surfaces.clear();
                    output.lenses[0].evidence.clear();
                }
                InvalidReviewCase::CheckedWithoutEvidence => {
                    output.lenses[0].evidence.clear();
                }
                InvalidReviewCase::NonContiguousFinding => {
                    output.clean = false;
                    let mut finding = valid_finding();
                    finding.finding_local_ordinal = 2;
                    output.findings.push(finding);
                }
                InvalidReviewCase::UnknownFindingRequirement => {
                    output.clean = false;
                    let mut finding = valid_finding();
                    finding.requirement_ids = vec!["unknown".to_string()];
                    output.findings.push(finding);
                }
                InvalidReviewCase::BlankFindingEvidence => {
                    output.clean = false;
                    let mut finding = valid_finding();
                    finding.concrete_evidence.clear();
                    output.findings.push(finding);
                }
                InvalidReviewCase::CleanFlagMismatch => output.clean = false,
                InvalidReviewCase::DispositionOnInitial => {
                    output
                        .original_finding_dispositions
                        .push(ReviewDisposition {
                            finding_id: "review-1/F1".to_string(),
                            disposition: FindingDisposition::Resolved,
                            evidence: "resolved".to_string(),
                        });
                }
            }
            assert!(
                validate_review_output(&dossier, output, false).is_none(),
                "case {case:?} unexpectedly validated"
            );
        }
    }

    #[test]
    fn rereview_dispositions_are_exact_and_unresolved_results_are_not_clean() {
        let mut dossier = dossier();
        dossier.original_findings = vec![CompletionReviewFindingReceipt {
            finding_id: "review-1/F1".to_string(),
            requirement_ids: vec!["requirement-1".to_string()],
            lens: REVIEW_LENSES[0].to_string(),
            contract_surface: "bounded owner".to_string(),
            severity: "high".to_string(),
            evidence: "missing behavior".to_string(),
            smallest_correction: "add behavior".to_string(),
            proof_route: "cargo test focused_case".to_string(),
        }];
        let resolved = ReviewDisposition {
            finding_id: "review-1/F1".to_string(),
            disposition: FindingDisposition::Resolved,
            evidence: "fresh proof covers the corrected behavior".to_string(),
        };
        let mut clean = clean_output();
        clean.original_finding_dispositions = vec![resolved.clone()];
        assert!(
            validate_review_output(&dossier, clean.clone(), true)
                .expect("clean rereview")
                .review_clean
        );

        let mut missing = clean.clone();
        missing.original_finding_dispositions.clear();
        assert!(validate_review_output(&dossier, missing, true).is_none());
        let mut duplicate = clean.clone();
        duplicate.original_finding_dispositions.push(resolved);
        assert!(validate_review_output(&dossier, duplicate, true).is_none());
        let mut unknown = clean.clone();
        unknown.original_finding_dispositions[0].finding_id = "review-1/F2".to_string();
        assert!(validate_review_output(&dossier, unknown, true).is_none());

        let mut still_present = clean;
        still_present.clean = false;
        still_present.original_finding_dispositions[0].disposition =
            FindingDisposition::StillPresent;
        let mut still_present_with_gap = still_present.clone();
        still_present_with_gap.sources[0].result = SourceReviewResultKind::ManifestGap;
        still_present_with_gap.sources[0].omitted_source_spans = vec![text_span(15, 24)];
        assert!(
            validate_review_output(&dossier, still_present_with_gap, true).is_none(),
            "a manifest gap cannot erase an unresolved original finding"
        );
        assert!(
            !validate_review_output(&dossier, still_present, true)
                .expect("unresolved rereview")
                .review_clean
        );
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

        let review: Value =
            serde_json::from_str(&review_dossier_json(&dossier, false)).expect("review JSON");
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
        let review_json = review_dossier_json(&dossier, false);
        assert!(!classification_json.contains(&image_payload));
        assert!(!review_json.contains(&image_payload));
        assert!(classification_json.contains(&reviewer_reference));
        assert!(review_json.contains(&reviewer_reference));

        for kind in [
            ReviewerRequestKind::Classification,
            ReviewerRequestKind::InitialReview,
        ] {
            let inputs = build_reviewer_inputs(&dossier, kind)
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

        let inputs = build_reviewer_inputs(&review_dossier, ReviewerRequestKind::Classification)
            .await
            .expect("the exact image-count limit should fit");
        assert_eq!(inputs.len(), MAX_RETAINED_USER_IMAGES + 1);

        let mut extra = review_dossier.sources[0].clone();
        extra.source_id = "source-over-limit".to_string();
        review_dossier.sources.push(extra);
        assert!(matches!(
            build_reviewer_inputs(&review_dossier, ReviewerRequestKind::Classification).await,
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
            build_reviewer_inputs(&review_dossier, ReviewerRequestKind::Classification).await,
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
