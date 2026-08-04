use chrono::Utc;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::ThreadId;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha1::Digest;
use sha1::Sha1;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tempfile::NamedTempFile;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tracing::warn;

const TASK_EVIDENCE_SCHEMA_VERSION: u32 = 6;
const FROZEN_TASK_EVIDENCE_V5_SCHEMA_VERSION: u32 = 5;
const FROZEN_TASK_EVIDENCE_V4_SCHEMA_VERSION: u32 = 4;
pub(crate) const SOURCE_CLASSIFICATION_CONTRACT_VERSION: &str = "source-local-v1";
pub(crate) const RELATIONSHIP_RESOLVER_CONTRACT_VERSION: &str = "relationship-v1";
const TASK_EVIDENCE_COMPLETION_MODEL_VERSION: u32 = 3;
const FILE_HASH_CHUNK_SIZE: usize = 64 * 1024;
const MAX_COMMAND_RECEIPTS: usize = 256;
const MAX_EDIT_RECEIPTS: usize = 256;
const MAX_EXTERNAL_EVIDENCE_RECEIPTS: usize = 256;
#[cfg(test)]
const MAX_COMPLETION_REVIEW_RECEIPTS: usize = 256;
const MAX_ATTRIBUTED_WORKSPACE_EVENTS: usize = 256;
const EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES: usize = 16 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES: usize = 8 * 1024;
const EXTERNAL_EVIDENCE_ARTIFACT_HEADER: &str =
    "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1\n";
const USER_SOURCE_LEDGER_CANONICAL_FORMAT: &str = "KD4_USER_SOURCE_LEDGER_CANONICAL_V1";
const REQUIREMENT_MANIFEST_CANONICAL_FORMAT: &str = "KD4_REQUIREMENT_MANIFEST_CANONICAL_V1";
const IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT: &str = "KD4_IMPLEMENTATION_IDENTITY_CANONICAL_V1";
const DOSSIER_SNAPSHOT_CANONICAL_FORMAT: &str = "KD4_DOSSIER_SNAPSHOT_CANONICAL_V1";
const REPAIR_INSTRUCTION_CANONICAL_FORMAT: &str = "KD4_REPAIR_INSTRUCTION_CANONICAL_V1";
const REPAIR_BASELINE_CANONICAL_FORMAT: &str = "KD4_REPAIR_BASELINE_CANONICAL_V1";
const REPAIR_DELTA_CANONICAL_FORMAT: &str = "KD4_REPAIR_DELTA_CANONICAL_V1";
const REREVIEW_AUDIT_CANONICAL_FORMAT: &str = "KD4_REREVIEW_AUDIT_CANONICAL_V1";
const REPAIR_PATH_GRAMMAR_VERSION: u32 = 1;
const REPAIR_RECURSIVE_WILDCARD_MAX_SEGMENTS: usize = 8;
pub(crate) const COMPLETION_REVIEW_LENSES: [&str; 8] = [
    "requirements_and_behavioral_compatibility",
    "lifecycle_and_concurrency",
    "persistence_filesystem_safety_rollback_and_atomicity",
    "schema_protocol_and_generated_representations",
    "security_and_trust_boundaries",
    "platform_configuration_packaging_and_installation",
    "pipeline_cache_snapshot_and_artifact_identity",
    "validation_quality_and_changed_test_oracle_integrity",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskEvidenceMode {
    Disabled,
    EvidenceOnly,
    Kd4Completion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalEvidenceCapture {
    Ignored,
    Stored,
    Warning(&'static str),
}

#[derive(Clone)]
pub(crate) struct TaskEvidenceLedger {
    mode: TaskEvidenceMode,
    codex_home: Option<PathBuf>,
    thread_id: Option<String>,
    evidence_path: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    document: Arc<Mutex<Option<TaskEvidenceDocument>>>,
    persistence_gate: Arc<Semaphore>,
    external_evidence_gate: Arc<Semaphore>,
    last_persisted_revision: Arc<AtomicU64>,
    source_capture_failed: Arc<AtomicBool>,
    #[cfg(test)]
    persistence_test_control: Arc<std::sync::Mutex<Option<PersistenceTestControl>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildEvidenceProvenance {
    pub(crate) source_thread_id: String,
    pub(crate) source_agent_path: String,
}

type PersistenceWriteBarrierPair = (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistOutcome {
    Persisted,
    Superseded,
    Failed,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
struct PersistenceTestControl {
    before_next_write: Arc<std::sync::Mutex<Option<PersistenceWriteBarrierPair>>>,
    fail_writes: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskEvidenceDocument {
    schema_version: u32,
    #[serde(default)]
    revision: u64,
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
    #[serde(default)]
    external_evidence: Vec<ExternalEvidenceReceipt>,
    #[serde(default)]
    completion_review_receipts: Vec<CompletionReviewAuditReceipt>,
    generated_artifact_requirements: Vec<GeneratedArtifactRequirement>,
    #[serde(default)]
    latest_generated_artifact_hashes: BTreeMap<String, FileHashSnapshot>,
    latest_file_hashes: BTreeMap<String, FileHashSnapshot>,
    risks: Vec<EvidenceRisk>,
    desktop_activation_receipt: Option<DesktopActivationReceipt>,
    #[serde(default = "initial_receipt_sequence")]
    next_edit_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_command_receipt_sequence: u64,
    #[serde(default = "initial_receipt_sequence")]
    next_external_evidence_receipt_sequence: u64,
    #[serde(default)]
    host_mutation_revision: u64,
    #[serde(default)]
    completion_review_v2: Option<CompletionReviewLedgerV2>,
    #[serde(default, deserialize_with = "deserialize_source_classification_cache")]
    source_classification_cache: Vec<SourceClassificationCacheEntry>,
    completion: Option<TaskCompletionGate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionReviewLedgerV2 {
    root_task_id: String,
    completion_epoch: u64,
    manifest_revision: u64,
    next_source_ordinal: u64,
    source_records: BTreeMap<String, UserSourceRecord>,
    mapping_revisions: Vec<SourceMappingRevision>,
    manifest_snapshots: Vec<RequirementManifestSnapshot>,
    active_review_cycle: Option<CompletionReviewCycle>,
    review_risk: CompletionReviewRisk,
    receipts: Vec<CompletionReviewReceiptV2>,
    next_review_sequence: u64,
    last_terminal_closure: Option<String>,
    #[serde(default)]
    last_workspace_event_epoch: u64,
    #[serde(default)]
    workspace_event_baseline_epoch: u64,
    #[serde(default)]
    typed_assignment_baseline: BTreeSet<String>,
    #[serde(default)]
    attributed_workspace_events: Vec<TaskAttributedWorkspaceEvent>,
    #[serde(default)]
    source_capture_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TaskAttributedWorkspaceEvent {
    workspace_id: String,
    epoch: u64,
    actor_id: String,
    paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserSourceRecord {
    pub(crate) source_id: String,
    pub(crate) message_id: String,
    pub(crate) source_kind: UserSourceKind,
    pub(crate) content_hash: String,
    pub(crate) source_ordinal: u64,
    pub(crate) content_ordinal: u64,
    pub(crate) exact_material: String,
    pub(crate) availability: UserSourceAvailability,
    pub(crate) completion_epoch: u64,
    #[serde(default)]
    pub(crate) introduced_manifest_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UserSourceKind {
    Text,
    Image,
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UserSourceAvailability {
    Available,
    Unavailable,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceMappingRevision {
    completion_epoch: u64,
    manifest_revision: u64,
    source_id: String,
    #[serde(default)]
    source_classification_contract_version: Option<String>,
    #[serde(default)]
    relationship_resolver_contract_version: Option<String>,
    mapping: SourceMapping,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SourceClassificationCacheKey {
    pub(crate) contract_version: String,
    pub(crate) source_kind: UserSourceKind,
    pub(crate) content_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceLocalClassificationKind {
    RequirementBearing,
    NonRequirement,
    RelationshipOnlyContext,
    UnavailableOrTruncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalSemanticCueKind {
    Assertion,
    ReplacementIntent,
    WithdrawalIntent,
    RelationshipOnlyContext,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalSemanticCue {
    pub(crate) kind: LocalSemanticCueKind,
    pub(crate) source_span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceLocalClassification {
    pub(crate) local_kind: SourceLocalClassificationKind,
    pub(crate) requirement_spans: Vec<SourceSpan>,
    pub(crate) local_semantic_cues: Vec<LocalSemanticCue>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceClassificationCacheEntry {
    pub(crate) contract_version: String,
    pub(crate) source_kind: UserSourceKind,
    pub(crate) content_hash: String,
    pub(crate) classification: SourceLocalClassification,
}

impl SourceClassificationCacheEntry {
    pub(crate) fn key(&self) -> SourceClassificationCacheKey {
        SourceClassificationCacheKey {
            contract_version: self.contract_version.clone(),
            source_kind: self.source_kind,
            content_hash: self.content_hash.clone(),
        }
    }
}

fn deserialize_source_classification_cache<'de, D>(
    deserializer: D,
) -> Result<Vec<SourceClassificationCacheEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    let mut by_key =
        BTreeMap::<SourceClassificationCacheKey, Option<SourceClassificationCacheEntry>>::new();
    for value in values {
        let Some(key) = source_classification_cache_key_from_value(&value) else {
            continue;
        };
        let entry = serde_json::from_value::<SourceClassificationCacheEntry>(value)
            .ok()
            .filter(source_classification_cache_entry_is_valid);
        match by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                slot.insert(None);
            }
        }
    }
    Ok(by_key.into_values().flatten().collect())
}

fn source_classification_cache_key_from_value(
    value: &Value,
) -> Option<SourceClassificationCacheKey> {
    let object = value.as_object()?;
    let key = SourceClassificationCacheKey {
        contract_version: object.get("contract_version")?.as_str()?.to_string(),
        source_kind: serde_json::from_value(object.get("source_kind")?.clone()).ok()?,
        content_hash: object.get("content_hash")?.as_str()?.to_string(),
    };
    source_classification_cache_key_is_valid(&key).then_some(key)
}

fn source_classification_cache_key_is_valid(key: &SourceClassificationCacheKey) -> bool {
    !key.contract_version.trim().is_empty()
        && key.content_hash.len() == 64
        && key
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn cache_span_is_structurally_valid(source_kind: UserSourceKind, span: &SourceSpan) -> bool {
    match (source_kind, span) {
        (UserSourceKind::Text, SourceSpan::Text { start, end }) => start < end,
        (UserSourceKind::Image, SourceSpan::Image { reference, .. }) => {
            !reference.trim().is_empty()
        }
        (UserSourceKind::Attachment, SourceSpan::Attachment { reference, .. }) => {
            !reference.trim().is_empty()
        }
        _ => false,
    }
}

fn source_classification_cache_entry_is_valid(entry: &SourceClassificationCacheEntry) -> bool {
    if !source_classification_cache_key_is_valid(&entry.key())
        || entry.classification.reason.trim().is_empty()
    {
        return false;
    }
    let spans = &entry.classification.requirement_spans;
    if spans
        .iter()
        .any(|span| !cache_span_is_structurally_valid(entry.source_kind, span))
        || spans.windows(2).any(|pair| pair[0] >= pair[1])
        || entry.classification.local_semantic_cues.iter().any(|cue| {
            cue.source_span
                .as_ref()
                .is_some_and(|span| !cache_span_is_structurally_valid(entry.source_kind, span))
        })
        || entry
            .classification
            .local_semantic_cues
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return false;
    }
    matches!(
        (entry.classification.local_kind, spans.is_empty()),
        (SourceLocalClassificationKind::RequirementBearing, false)
            | (SourceLocalClassificationKind::NonRequirement, true)
            | (SourceLocalClassificationKind::RelationshipOnlyContext, true)
            | (SourceLocalClassificationKind::UnavailableOrTruncated, true)
    )
}

fn canonical_source_classification_cache(
    entries: Vec<SourceClassificationCacheEntry>,
) -> Vec<SourceClassificationCacheEntry> {
    let mut by_key =
        BTreeMap::<SourceClassificationCacheKey, Option<SourceClassificationCacheEntry>>::new();
    for entry in entries {
        let key = entry.key();
        match by_key.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(source_classification_cache_entry_is_valid(&entry).then_some(entry));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                slot.insert(None);
            }
        }
    }
    by_key.into_values().flatten().collect()
}

pub(crate) fn source_classification_cache_key(
    source: &UserSourceRecord,
) -> SourceClassificationCacheKey {
    SourceClassificationCacheKey {
        contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
        source_kind: source.source_kind,
        content_hash: source.content_hash.clone(),
    }
}

pub(crate) fn source_local_classification_is_valid_for_source(
    source: &UserSourceRecord,
    classification: &SourceLocalClassification,
) -> bool {
    let entry = SourceClassificationCacheEntry {
        contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
        source_kind: source.source_kind,
        content_hash: source.content_hash.clone(),
        classification: classification.clone(),
    };
    source_classification_cache_entry_is_valid(&entry)
        && classification
            .requirement_spans
            .iter()
            .chain(
                classification
                    .local_semantic_cues
                    .iter()
                    .filter_map(|cue| cue.source_span.as_ref()),
            )
            .all(|span| material_for_span(source, span).is_some())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceMapping {
    PendingClassification,
    RequirementBearing { requirement_ids: Vec<String> },
    NonRequirement { reason: String },
    SupersededContext { reason: String },
    UnavailableOrTruncated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RequirementManifestSnapshot {
    completion_epoch: u64,
    manifest_revision: u64,
    manifest_hash: String,
    requirements: Vec<RequirementRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequirementRecord {
    pub(crate) requirement_id: String,
    pub(crate) source_id: String,
    pub(crate) source_content_hash: String,
    pub(crate) source_span: SourceSpan,
    pub(crate) exact_material: String,
    pub(crate) status: RequirementStatus,
    pub(crate) superseded_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SourceSpan {
    Text {
        start: usize,
        end: usize,
    },
    Image {
        reference: String,
        region: Option<String>,
    },
    Attachment {
        reference: String,
        range: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequirementStatus {
    Active,
    Superseded,
    Withdrawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassifiedSourceKind {
    RequirementBearing,
    NonRequirement,
    SupersededContext,
    UnavailableOrTruncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedSource {
    pub(crate) source_id: String,
    pub(crate) kind: ClassifiedSourceKind,
    pub(crate) requirements: Vec<ClassifiedRequirement>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceMaterialization {
    /// Exactly one source-local projection for every unique current cache key.
    pub(crate) local_classifications:
        BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    /// Exactly one occurrence-specific relationship result for every current source.
    pub(crate) resolved_sources: Vec<ClassifiedSource>,
}

#[derive(Debug)]
struct PreparedSourceMaterialization {
    local_classifications: BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    requirements: Vec<RequirementRecord>,
    mappings: Vec<(String, SourceMapping)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedRequirement {
    pub(crate) source_span: SourceSpan,
    pub(crate) status: RequirementStatus,
    pub(crate) superseded_by: Option<ClassifiedRequirementRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ClassifiedRequirementRef {
    pub(crate) source_id: String,
    pub(crate) source_span: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewCycle {
    cycle_id: String,
    manifest_revision: u64,
    parent_terminal_review_id: Option<String>,
    #[serde(default)]
    superseded_review_id: Option<String>,
    phase: CompletionReviewCyclePhase,
    correction_consumed: bool,
    #[serde(default)]
    manifest_gap_reconstructed: bool,
    accepted_review_id: Option<String>,
    accepted_dossier_snapshot_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewCyclePhase {
    ClassificationPending,
    InitialReviewPending,
    CorrectionPending,
    RereviewPending,
    ProvisionalClean,
    TerminalPartial,
    TerminalBlocked,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewRisk {
    unresolved: bool,
    cycle_id: Option<String>,
    opened_at: Option<String>,
    resolved_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionReviewAttemptKind {
    InitialReview,
    CorrectionEvidence,
    Rereview,
    TerminalClosure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReviewFindingReceipt {
    pub(crate) finding_id: String,
    pub(crate) requirement_ids: Vec<String>,
    pub(crate) lens: String,
    pub(crate) contract_surface: String,
    pub(crate) severity: String,
    pub(crate) evidence: String,
    pub(crate) smallest_correction: String,
    pub(crate) proof_route: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionReviewDispositionReceipt {
    pub(crate) finding_id: String,
    pub(crate) disposition: String,
    pub(crate) evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RereviewFallbackReason {
    MissingBaseline,
    InvalidBaselineHash,
    InvalidRepairLineage,
    UnsupportedPathGrammar,
    InvalidPath,
    AmbiguousWindowsCase,
    SymlinkEscape,
    PathOutsideScope,
    SourceIdentityChanged,
    RequirementManifestChanged,
    PlanStructureChanged,
    ContractSurfaceOutsideScope,
    UnattributedMutation,
    UnrepresentableEvidenceChange,
    CommandLineageChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RereviewInputMode {
    Delta,
    FullFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StructuredContractSurface {
    pub(crate) kind: String,
    pub(crate) owner: String,
    pub(crate) identifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RepairPathScope {
    ExactFile {
        path: String,
    },
    DirectoryPrefix {
        path: String,
    },
    GeneratedPattern {
        grammar_version: u32,
        pattern: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairPathState {
    pub(crate) path: String,
    pub(crate) exists: bool,
    pub(crate) content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BaselineCommandBinding {
    pub(crate) sequence: u64,
    pub(crate) receipt_id: String,
    pub(crate) implementation_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairScope {
    pub(crate) path_grammar_version: u32,
    pub(crate) paths: Vec<RepairPathScope>,
    pub(crate) surfaces: Vec<StructuredContractSurface>,
    pub(crate) affected_requirement_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairBaseline {
    pub(crate) path_states: Vec<RepairPathState>,
    pub(crate) command_sequence_high_water_mark: u64,
    pub(crate) command_bindings: Vec<BaselineCommandBinding>,
    pub(crate) implementation_surfaces: Vec<StructuredContractSurface>,
    pub(crate) repair_scope: RepairScope,
    pub(crate) source_ledger_hash: String,
    pub(crate) requirement_manifest_hash: String,
    pub(crate) plan_structure_hash: String,
    pub(crate) default_child_mutation_identities: Vec<String>,
    pub(crate) typed_mutation_identities: Vec<String>,
    pub(crate) external_evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairPathChangeKind {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairPathChange {
    pub(crate) path: String,
    pub(crate) change: RepairPathChangeKind,
    pub(crate) before_exists: bool,
    pub(crate) before_hash: Option<String>,
    pub(crate) after_exists: bool,
    pub(crate) after_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairCommandDelta {
    pub(crate) sequence: u64,
    pub(crate) receipt_id: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) implementation_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InvalidatedCommandReceipt {
    pub(crate) sequence: u64,
    pub(crate) receipt_id: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairDelta {
    pub(crate) original_findings: Vec<CompletionReviewFindingReceipt>,
    pub(crate) required_disposition_finding_ids: Vec<String>,
    pub(crate) repair_instruction_hash: String,
    pub(crate) baseline_hash: String,
    pub(crate) candidate_implementation_identity: String,
    pub(crate) path_changes: Vec<RepairPathChange>,
    pub(crate) new_command_receipts: Vec<RepairCommandDelta>,
    pub(crate) invalidated_command_receipts: Vec<InvalidatedCommandReceipt>,
    pub(crate) affected_requirement_ids: Vec<String>,
    pub(crate) newly_realized_surfaces: Vec<StructuredContractSurface>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RereviewInput {
    pub(crate) input_mode: RereviewInputMode,
    pub(crate) baseline_hash: Option<String>,
    pub(crate) delta_hash: Option<String>,
    pub(crate) fallback_reasons: Vec<RereviewFallbackReason>,
    pub(crate) repair_instruction_hash: String,
    pub(crate) candidate_implementation_identity: String,
    pub(crate) delta: Option<RepairDelta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentRepairSnapshot {
    pub(crate) repository_root: String,
    pub(crate) path_states: Vec<RepairPathState>,
    pub(crate) command_receipts: Vec<RepairCommandDelta>,
    pub(crate) plan_structure_hash: String,
    pub(crate) declared_path_scopes: Vec<RepairPathScope>,
    pub(crate) implementation_surfaces: Vec<StructuredContractSurface>,
    pub(crate) default_child_mutation_identities: Vec<String>,
    pub(crate) typed_mutation_identities: Vec<String>,
    pub(crate) external_evidence_ids: Vec<String>,
    pub(crate) containment_errors: Vec<RereviewFallbackReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CompletionReviewReceiptV2 {
    review_id: String,
    attempt_kind: CompletionReviewAttemptKind,
    parent_review_id: Option<String>,
    superseded_review_id: Option<String>,
    candidate_mutation_revision: u64,
    candidate_hash: String,
    implementation_identity_hash: String,
    dossier_snapshot_id: String,
    user_source_ledger_hash: String,
    requirement_manifest_hash: String,
    findings: Vec<CompletionReviewFindingReceipt>,
    dispositions: Vec<CompletionReviewDispositionReceipt>,
    #[serde(default)]
    manifest_gaps: Vec<ManifestGapInput>,
    repair_instruction_hash: Option<String>,
    #[serde(default)]
    repair_baseline: Option<RepairBaseline>,
    #[serde(default)]
    baseline_hash: Option<String>,
    #[serde(default)]
    input_mode: Option<RereviewInputMode>,
    #[serde(default)]
    delta_hash: Option<String>,
    #[serde(default)]
    rereview_delta: Option<RepairDelta>,
    #[serde(default)]
    fallback_reasons: Vec<RereviewFallbackReason>,
    #[serde(default)]
    candidate_implementation_identity: Option<String>,
    #[serde(default)]
    rereview_audit_hash: Option<String>,
    infrastructure_outcome: String,
    review_clean: bool,
    terminal_outcome: Option<String>,
    recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewFindingInput {
    pub(crate) local_ordinal: u32,
    pub(crate) requirement_ids: Vec<String>,
    pub(crate) lens: String,
    pub(crate) contract_surface: String,
    pub(crate) severity: String,
    pub(crate) evidence: String,
    pub(crate) smallest_correction: String,
    pub(crate) proof_route: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewAttemptInput {
    pub(crate) attempt_kind: CompletionReviewAttemptKind,
    pub(crate) parent_review_id: Option<String>,
    pub(crate) superseded_review_id: Option<String>,
    pub(crate) findings: Vec<CompletionReviewFindingInput>,
    pub(crate) dispositions: Vec<CompletionReviewDispositionReceipt>,
    pub(crate) manifest_gaps: Vec<ManifestGapInput>,
    pub(crate) repair_instruction: Option<String>,
    pub(crate) repair_instruction_hash: Option<String>,
    pub(crate) infrastructure_outcome: String,
    pub(crate) review_clean: bool,
    pub(crate) terminal_outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedReviewAttempt {
    pub(crate) review_id: String,
    pub(crate) findings: Vec<CompletionReviewFindingReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestGapInput {
    pub(crate) source_id: String,
    pub(crate) omitted_spans: Vec<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionReviewDossier {
    pub(crate) document_revision: u64,
    pub(crate) root_task_id: String,
    pub(crate) completion_epoch: u64,
    pub(crate) manifest_revision: u64,
    pub(crate) sources: Vec<UserSourceRecord>,
    pub(crate) source_mappings: BTreeMap<String, SourceMapping>,
    pub(crate) source_classification_cache:
        BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>,
    pub(crate) source_classification_current: bool,
    pub(crate) relationship_resolution_current: bool,
    pub(crate) mappings_classified: bool,
    pub(crate) source_capture_failed: bool,
    pub(crate) requirements: Vec<RequirementRecord>,
    pub(crate) user_source_ledger_hash: String,
    pub(crate) requirement_manifest_hash: String,
    pub(crate) implementation_identity_hash: String,
    pub(crate) dossier_snapshot_id: String,
    pub(crate) host_mutation_revision: u64,
    pub(crate) has_task_attributed_mutations: bool,
    pub(crate) evidence_gate: TaskCompletionGate,
    pub(crate) locally_obtainable_proof_routes: Vec<String>,
    pub(crate) reviewer_visible_evidence: Value,
    pub(crate) review_lens_selection_facts: ReviewLensSelectionFacts,
    pub(crate) authoritative_input_errors: Vec<String>,
    pub(crate) typed_quiescent: bool,
    pub(crate) default_children_quiescent: bool,
    pub(crate) candidate_completion: Option<String>,
    pub(crate) correction_consumed: bool,
    pub(crate) cycle_phase: Option<CompletionReviewCyclePhase>,
    pub(crate) active_cycle_id: Option<String>,
    pub(crate) cycle_parent_review_id: Option<String>,
    pub(crate) cycle_superseded_review_id: Option<String>,
    pub(crate) accepted_review_id: Option<String>,
    pub(crate) initial_review_id: Option<String>,
    pub(crate) initial_repair_instruction_hash: Option<String>,
    pub(crate) original_findings: Vec<CompletionReviewFindingReceipt>,
    pub(crate) manifest_gap_reconstructed: bool,
    pub(crate) current_repair_snapshot: CurrentRepairSnapshot,
    pub(crate) initial_repair_baseline: Option<RepairBaseline>,
    pub(crate) initial_repair_baseline_hash: Option<String>,
    pub(crate) rereview_input: Option<RereviewInput>,
}

/// Validated structured facts that the completion-review host may use to select
/// applicable review lenses. Free-form review material is intentionally absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReviewLensSelectionFacts {
    pub(crate) risk_hints: Vec<String>,
    pub(crate) task_mutation_paths: Vec<String>,
    pub(crate) child_mutation_paths: Vec<String>,
    pub(crate) plan_edit_paths: Vec<String>,
    pub(crate) plan_runtime_paths: Vec<String>,
    pub(crate) surface_roles: Vec<String>,
    pub(crate) validation_asset_paths: Vec<String>,
    pub(crate) generated_artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AtomicReviewTransition<T> {
    Persisted(T),
    Superseded,
    Failed,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EditIntent {
    call_id: String,
    step_id: Option<String>,
    started_at: String,
    completed_at: Option<String>,
    outcome: Option<String>,
    files: Vec<FileHashSnapshot>,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
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
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileHashTransition {
    path: String,
    before_sha1: Option<String>,
    after_sha1: Option<String>,
    before_exists: bool,
    after_exists: bool,
    #[serde(default)]
    before_read_error: Option<String>,
    #[serde(default)]
    after_read_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileHashSnapshot {
    path: String,
    sha1: Option<String>,
    exists: bool,
    #[serde(default)]
    read_error: Option<String>,
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
    #[serde(default)]
    host_mutation_revision: Option<u64>,
    #[serde(default)]
    manifest_revision: Option<u64>,
    #[serde(default)]
    user_source_ledger_hash: Option<String>,
    #[serde(default)]
    requirement_manifest_hash: Option<String>,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceCompleteness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalEvidenceReceipt {
    id: String,
    producer: String,
    producer_schema_version: u32,
    server_name: String,
    tool_name: String,
    call_id: String,
    #[serde(default)]
    source_thread_id: Option<String>,
    #[serde(default)]
    source_agent_path: Option<String>,
    recorded_at: String,
    task_epoch: u64,
    step_id: Option<String>,
    workspace_root_fingerprint: String,
    host_mutation_revision: Option<u64>,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
    provider_snapshot: Option<String>,
    tool_success: bool,
    payload_completeness: EvidenceCompleteness,
    truncated: bool,
    approximate: bool,
    limitations: Vec<String>,
    result_sha256: String,
    payload: Option<Value>,
    payload_artifact_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionReviewAuditReceipt {
    turn_id: String,
    recorded_at: String,
    evidence_epoch: u64,
    outcome: String,
    failure_category: Option<String>,
    finding_summary: Vec<String>,
    repair_injected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GeneratedArtifactRequirement {
    id: String,
    step_id: Option<String>,
    path: Option<String>,
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
struct DesktopActivationReceipt {
    epoch: u64,
    #[serde(default, alias = "recorded_at")]
    activation_timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binary_sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_evidence: Option<String>,
    #[serde(default)]
    expected_installed_executable_path: String,
    #[serde(default)]
    installed_executable_sha256: String,
    #[serde(default)]
    running_process_id: u32,
    #[serde(default)]
    running_process_identity: String,
    #[serde(default)]
    observed_running_executable_path: String,
    #[serde(default)]
    desktop_process_id: u32,
    #[serde(default)]
    desktop_process_identity: String,
    #[serde(default)]
    desktop_executable_path: String,
    #[serde(default)]
    post_restart_initialization_observation: String,
    #[serde(default)]
    observation_timestamp: String,
    #[serde(default)]
    implementation_identity_hash: Option<String>,
}

fn new_completion_review_ledger(root_task_id: &str) -> CompletionReviewLedgerV2 {
    CompletionReviewLedgerV2 {
        root_task_id: root_task_id.to_string(),
        completion_epoch: 1,
        manifest_revision: 0,
        next_source_ordinal: 1,
        source_records: BTreeMap::new(),
        mapping_revisions: Vec::new(),
        manifest_snapshots: Vec::new(),
        active_review_cycle: None,
        review_risk: CompletionReviewRisk {
            unresolved: false,
            cycle_id: None,
            opened_at: None,
            resolved_at: None,
        },
        receipts: Vec::new(),
        next_review_sequence: 1,
        last_terminal_closure: None,
        last_workspace_event_epoch: 0,
        workspace_event_baseline_epoch: 0,
        typed_assignment_baseline: BTreeSet::new(),
        attributed_workspace_events: Vec::new(),
        source_capture_failed: false,
    }
}

impl TaskEvidenceLedger {
    pub(crate) async fn load_or_new(codex_home: PathBuf, thread_id: ThreadId, cwd: &Path) -> Self {
        let (mode, repo_root) = if let Some(repo_root) = find_kd4_repo_root(cwd) {
            (TaskEvidenceMode::Kd4Completion, repo_root)
        } else if let Some(repo_root) = get_git_repo_root(cwd) {
            (TaskEvidenceMode::EvidenceOnly, repo_root)
        } else {
            return Self::disabled();
        };
        let repo_root = canonical_repository_root(&repo_root);
        let evidence_path = codex_home
            .join("task-evidence")
            .join(format!("{thread_id}.json"));
        let now = timestamp();
        let thread_id_text = thread_id.to_string();
        let repository_root = repo_root.to_string_lossy().into_owned();

        let existing = load_existing_document(&evidence_path, &thread_id_text, &repo_root).await;
        let mut storage_failure_reason = None;
        let mut recovery_failure_reason = None;
        let existing = match existing {
            ExistingDocument::Loaded {
                document,
                legacy_completion_model,
            } => Some((*document, legacy_completion_model)),
            ExistingDocument::Missing => None,
            ExistingDocument::NewerSchema { schema_version } => {
                warn!(
                    "disabling task evidence because {} uses newer schema version {schema_version}; refusing to modify it",
                    evidence_path.display()
                );
                return Self::disabled();
            }
            ExistingDocument::Rejected { kind, reason } => {
                recovery_failure_reason = Some(format!(
                    "rejected task evidence was removed from the active lineage: {reason}"
                ));
                let quarantine = quarantine_evidence_file(&evidence_path, kind).await;
                match quarantine {
                    Ok(path) => warn!(
                        "preserved rejected KD4 task evidence at {}: {reason}",
                        path.display()
                    ),
                    Err(err) => {
                        let failure = format!(
                            "rejected task evidence could not be quarantined ({reason}; quarantine failed: {err})"
                        );
                        warn!(
                            "refusing to overwrite rejected KD4 task evidence at {}: {failure}",
                            evidence_path.display()
                        );
                        storage_failure_reason = Some(failure);
                    }
                }
                None
            }
        };
        let document = if let Some((mut document, legacy_completion_model)) = existing {
            migrate_document_with_completion_model(&mut document, legacy_completion_model);
            document.start.repository_root.clone_from(&repository_root);
            document.updated_at = now;
            document.revision = document.revision.saturating_add(1);
            document
        } else {
            let git = collect_git_info(&repo_root).await;
            let mut risks = Vec::new();
            if let Some(reason) = storage_failure_reason.as_deref() {
                risks.push(task_evidence_storage_risk(reason, 0));
            }
            if let Some(reason) = recovery_failure_reason.as_deref() {
                risks.push(task_evidence_recovery_risk(reason, 0));
            }
            TaskEvidenceDocument {
                schema_version: TASK_EVIDENCE_SCHEMA_VERSION,
                revision: 1,
                thread_id: thread_id_text.clone(),
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
                external_evidence: Vec::new(),
                completion_review_receipts: Vec::new(),
                generated_artifact_requirements: Vec::new(),
                latest_generated_artifact_hashes: BTreeMap::new(),
                latest_file_hashes: BTreeMap::new(),
                risks,
                desktop_activation_receipt: None,
                next_edit_receipt_sequence: initial_receipt_sequence(),
                next_command_receipt_sequence: initial_receipt_sequence(),
                next_external_evidence_receipt_sequence: initial_receipt_sequence(),
                host_mutation_revision: 0,
                completion_review_v2: Some(new_completion_review_ledger(&thread_id_text)),
                source_classification_cache: Vec::new(),
                completion: None,
            }
        };
        if mode == TaskEvidenceMode::EvidenceOnly && storage_failure_reason.is_some() {
            warn!(
                "disabling evidence-only task ledger because rejected evidence could not be safely replaced"
            );
            return Self::disabled();
        }
        let writable_evidence_path = storage_failure_reason.is_none().then_some(evidence_path);
        let source_capture_failed = document
            .completion_review_v2
            .as_ref()
            .is_some_and(|ledger| ledger.source_capture_failed);
        let ledger = Self {
            mode,
            codex_home: Some(codex_home.clone()),
            thread_id: Some(thread_id_text.clone()),
            evidence_path: writable_evidence_path,
            repo_root: Some(repo_root),
            document: Arc::new(Mutex::new(Some(document.clone()))),
            persistence_gate: Arc::new(Semaphore::new(1)),
            external_evidence_gate: Arc::new(Semaphore::new(1)),
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            source_capture_failed: Arc::new(AtomicBool::new(source_capture_failed)),
            #[cfg(test)]
            persistence_test_control: Arc::new(std::sync::Mutex::new(None)),
        };
        if storage_failure_reason.is_none() {
            let persisted = ledger.persist_document(&document).await;
            if mode == TaskEvidenceMode::EvidenceOnly && persisted != PersistOutcome::Persisted {
                warn!(
                    "disabling evidence-only task ledger because initial persistence could not be established"
                );
                return Self::disabled();
            }
            if mode == TaskEvidenceMode::Kd4Completion && persisted == PersistOutcome::Failed {
                let mut guard = ledger.document.lock().await;
                if let Some(document) = guard.as_mut() {
                    upsert_risk(
                        document,
                        task_evidence_storage_risk(
                            "initial task-evidence document could not be durably persisted",
                            document.evidence_epoch,
                        ),
                    );
                    document.updated_at = timestamp();
                    document.revision = document.revision.saturating_add(1);
                }
            }
        }
        let referenced_artifact_ids = document
            .external_evidence
            .iter()
            .filter_map(|receipt| receipt.payload_artifact_id.clone())
            .collect();
        let live_artifact_ids =
            crate::tools::command_output_artifact::reconcile_evidence_artifact_protection(
                &codex_home,
                &thread_id_text,
                &referenced_artifact_ids,
            )
            .await;
        let repaired_snapshot = {
            let mut guard = ledger.document.lock().await;
            if let Some(document) = guard.as_mut() {
                let before = document.external_evidence.len();
                document.external_evidence.retain(|receipt| {
                    receipt
                        .payload_artifact_id
                        .as_ref()
                        .is_none_or(|artifact_id| live_artifact_ids.contains(artifact_id))
                });
                let removed = before.saturating_sub(document.external_evidence.len());
                if removed == 0 {
                    None
                } else {
                    warn!(
                        "removed {removed} external evidence receipt(s) whose payload artifacts were missing or invalid"
                    );
                    document.updated_at = timestamp();
                    document.revision = document.revision.saturating_add(1);
                    Some(document.clone())
                }
            } else {
                None
            }
        };
        if let Some(repaired_snapshot) = repaired_snapshot
            && ledger.persist_document(&repaired_snapshot).await != PersistOutcome::Persisted
        {
            warn!("failed to persist repaired external evidence receipts");
        }
        ledger
    }

    pub(crate) fn disabled() -> Self {
        Self {
            mode: TaskEvidenceMode::Disabled,
            codex_home: None,
            thread_id: None,
            evidence_path: None,
            repo_root: None,
            document: Arc::new(Mutex::new(None)),
            persistence_gate: Arc::new(Semaphore::new(1)),
            external_evidence_gate: Arc::new(Semaphore::new(1)),
            last_persisted_revision: Arc::new(AtomicU64::new(0)),
            source_capture_failed: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            persistence_test_control: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn mode(&self) -> TaskEvidenceMode {
        self.mode
    }

    pub(crate) fn allows_kd4_completion(&self) -> bool {
        self.mode == TaskEvidenceMode::Kd4Completion
    }

    pub(crate) async fn mark_user_source_capture_failed(&self) {
        self.source_capture_failed.store(true, Ordering::Release);
        if !self.allows_kd4_completion() {
            return;
        }
        let Some((_, snapshot)) = self
            .update_document(|document| {
                let root_task_id = document.thread_id.clone();
                let ledger = document
                    .completion_review_v2
                    .get_or_insert_with(|| new_completion_review_ledger(&root_task_id));
                ledger.source_capture_failed = true;
                ledger.review_risk.unresolved = true;
                ledger.review_risk.resolved_at = None;
                document.completion = None;
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        if self.persist_document(&snapshot).await != PersistOutcome::Persisted {
            warn!("failed to durably persist user-source capture failure");
        }
    }

    pub(crate) fn user_source_capture_failed(&self) -> bool {
        self.source_capture_failed.load(Ordering::Acquire)
    }

    pub(crate) fn matches_repo_root(&self, candidate: &Path) -> bool {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        repository_roots_match(repo_root, candidate)
    }

    pub(crate) fn repository_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
    }

    pub(crate) async fn last_workspace_event_epoch(&self) -> u64 {
        let guard = self.document.lock().await;
        guard
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .map(|ledger| ledger.last_workspace_event_epoch)
            .unwrap_or_default()
    }

    pub(crate) async fn typed_assignment_baseline(&self) -> BTreeSet<String> {
        let guard = self.document.lock().await;
        guard
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .map(|ledger| ledger.typed_assignment_baseline.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn seed_workspace_event_baseline(
        &self,
        workspace_epoch: u64,
        typed_assignment_baseline: BTreeSet<String>,
    ) -> bool {
        if !self.allows_kd4_completion() {
            return true;
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return false;
            };
            let typed_assignment_baseline = typed_assignment_baseline.clone();
            match self
                .atomic_review_update(expected_revision, None, None, move |document| {
                    let Some(ledger) = document.completion_review_v2.as_ref() else {
                        return false;
                    };
                    if ledger.workspace_event_baseline_epoch == ledger.completion_epoch {
                        return true;
                    }
                    let completion_epoch = ledger.completion_epoch;
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        return false;
                    };
                    ledger.last_workspace_event_epoch = workspace_epoch;
                    ledger.workspace_event_baseline_epoch = completion_epoch;
                    ledger.typed_assignment_baseline = typed_assignment_baseline;
                    resolve_risk(document, "completion-review-workspace-baseline");
                    document.updated_at = timestamp();
                    true
                })
                .await
            {
                AtomicReviewTransition::Persisted(result) => return result,
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return false,
            }
        }
        false
    }

    pub(crate) async fn mark_workspace_event_baseline_failed(&self, reason: &str) {
        if !self.allows_kd4_completion() {
            return;
        }
        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return;
            };
            let reason = reason.to_string();
            match self
                .atomic_review_update(expected_revision, None, None, move |document| {
                    let epoch = document.evidence_epoch;
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: "completion-review-workspace-baseline".to_string(),
                            description: reason,
                            source: "workspace-event-baseline".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
                    document.completion = None;
                    document.updated_at = timestamp();
                })
                .await
            {
                AtomicReviewTransition::Persisted(()) => return,
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return,
            }
        }
    }

    pub(crate) async fn record_user_sources(
        &self,
        message_id: &str,
        content: &[UserInput],
    ) -> bool {
        if !self.allows_kd4_completion() {
            return false;
        }
        let mut source_materials = Vec::new();
        for (index, input) in content.iter().enumerate() {
            if let Some((kind, exact_material, availability)) = user_source_material(input).await {
                source_materials.push((index as u64, kind, exact_material, availability));
            }
        }
        if source_materials.is_empty() {
            return true;
        }

        for _ in 0..8 {
            let Some(expected_revision) = self.document_revision().await else {
                return false;
            };
            let outcome = self
                .atomic_review_update(expected_revision, None, None, |document| {
                    let root_task_id = document.thread_id.clone();
                    let ledger = document
                        .completion_review_v2
                        .get_or_insert_with(|| new_completion_review_ledger(&root_task_id));
                    if ledger
                        .active_review_cycle
                        .as_ref()
                        .is_some_and(|cycle| cycle.phase == CompletionReviewCyclePhase::Closed)
                    {
                        ledger.completion_epoch = ledger.completion_epoch.saturating_add(1);
                        ledger.manifest_revision = 0;
                        ledger.active_review_cycle = None;
                        ledger.attributed_workspace_events.clear();
                        ledger.source_capture_failed = false;
                        ledger.review_risk = CompletionReviewRisk {
                            unresolved: false,
                            cycle_id: None,
                            opened_at: None,
                            resolved_at: None,
                        };
                        document.host_mutation_revision = 0;
                        document.latest_file_hashes.clear();
                        document.latest_generated_artifact_hashes.clear();
                        document.plan.clear();
                        document.active_step_id = None;
                        document.risks.clear();
                        document.completion = None;
                    }
                    let mut inserted = false;
                    let mut inserted_ids = BTreeSet::new();
                    for (content_ordinal, kind, exact_material, availability) in &source_materials {
                        let content_hash =
                            user_source_content_hash(*kind, exact_material, *availability);
                        let source_id = deterministic_source_id(
                            &ledger.root_task_id,
                            ledger.completion_epoch,
                            message_id,
                            *content_ordinal,
                            &content_hash,
                        );
                        if ledger.source_records.contains_key(&source_id) {
                            continue;
                        }
                        let source_ordinal = ledger.next_source_ordinal;
                        ledger.next_source_ordinal = ledger.next_source_ordinal.saturating_add(1);
                        ledger.source_records.insert(
                            source_id.clone(),
                            UserSourceRecord {
                                source_id: source_id.clone(),
                                message_id: message_id.to_string(),
                                source_kind: *kind,
                                content_hash,
                                source_ordinal,
                                content_ordinal: *content_ordinal,
                                exact_material: exact_material.clone(),
                                availability: *availability,
                                completion_epoch: ledger.completion_epoch,
                                introduced_manifest_revision: ledger
                                    .manifest_revision
                                    .saturating_add(1),
                            },
                        );
                        inserted_ids.insert(source_id);
                        inserted = true;
                    }
                    if !inserted {
                        return true;
                    }

                    let previous_mappings = active_source_mappings(ledger);
                    let previous_requirements = active_manifest(ledger)
                        .map(|manifest| manifest.requirements.clone())
                        .unwrap_or_default();
                    ledger.manifest_revision = ledger.manifest_revision.saturating_add(1);
                    let manifest_revision = ledger.manifest_revision;
                    let active_source_ids = ledger
                        .source_records
                        .values()
                        .filter(|source| source.completion_epoch == ledger.completion_epoch)
                        .map(|source| source.source_id.clone())
                        .collect::<Vec<_>>();
                    for source_id in active_source_ids {
                        let mapping = if inserted_ids.contains(&source_id) {
                            SourceMapping::PendingClassification
                        } else {
                            previous_mappings
                                .get(&source_id)
                                .cloned()
                                .unwrap_or(SourceMapping::PendingClassification)
                        };
                        ledger.mapping_revisions.push(SourceMappingRevision {
                            completion_epoch: ledger.completion_epoch,
                            manifest_revision,
                            source_id,
                            source_classification_contract_version: None,
                            relationship_resolver_contract_version: None,
                            mapping,
                        });
                    }
                    let manifest_hash =
                        requirement_manifest_hash(manifest_revision, &previous_requirements);
                    ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                        completion_epoch: ledger.completion_epoch,
                        manifest_revision,
                        manifest_hash,
                        requirements: previous_requirements,
                    });
                    let cycle_id = format!(
                        "cycle-{}-{}",
                        ledger.completion_epoch, ledger.manifest_revision
                    );
                    let parent_terminal_review_id = ledger
                        .receipts
                        .iter()
                        .rev()
                        .find(|receipt| {
                            receipt.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
                        })
                        .map(|receipt| receipt.review_id.clone());
                    ledger.active_review_cycle = Some(CompletionReviewCycle {
                        cycle_id,
                        manifest_revision,
                        parent_terminal_review_id,
                        superseded_review_id: None,
                        phase: CompletionReviewCyclePhase::ClassificationPending,
                        correction_consumed: false,
                        manifest_gap_reconstructed: false,
                        accepted_review_id: None,
                        accepted_dossier_snapshot_id: None,
                    });
                    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
                    document.desktop_activation_receipt = None;
                    document.completion = None;
                    document.updated_at = timestamp();
                    true
                })
                .await;
            match outcome {
                AtomicReviewTransition::Persisted(result) => {
                    let source_capture_failed = self
                        .document
                        .lock()
                        .await
                        .as_ref()
                        .and_then(|document| document.completion_review_v2.as_ref())
                        .is_some_and(|ledger| ledger.source_capture_failed);
                    self.source_capture_failed
                        .store(source_capture_failed, Ordering::Release);
                    return result;
                }
                AtomicReviewTransition::Superseded => continue,
                AtomicReviewTransition::Failed => return false,
            }
        }
        false
    }

    pub(crate) async fn record_plan_update(&self, update: &UpdatePlanArgs) -> UpdatePlanArgs {
        if !self.allows_kd4_completion() {
            return update.clone();
        }
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
                let mut material_plan_change = previous.len() != update.plan.len();
                let mut duplicate_explicit_ids = BTreeSet::new();
                let mut seen_explicit_ids = BTreeSet::new();
                for (index, item) in update.plan.iter().enumerate() {
                    if let Some(id) = item.id.as_ref()
                        && !seen_explicit_ids.insert(id.clone())
                    {
                        duplicate_explicit_ids.insert(id.clone());
                    }
                    let id = effective_step_id(item, index, &mut used_ids);
                    let old = previous.get(&id);
                    let material_step_change =
                        old.is_none_or(|step| !step_materially_matches_item(step, item));
                    material_plan_change |= material_step_change;
                    let status = normalize_requested_status(&item.status);
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
                        edit_paths: old
                            .filter(|_| !material_step_change)
                            .map_or_else(BTreeSet::new, |step| step.edit_paths.clone()),
                    });
                }
                material_plan_change |= previous
                    .keys()
                    .any(|id| !normalized.iter().any(|step| &step.id == id));
                document.plan = normalized;
                if material_plan_change {
                    invalidate_for_plan_change(document);
                }
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                rebuild_declared_requirements_and_risks(document);
                sync_plan_structure_state(document, &duplicate_explicit_ids);
                if plan_is_terminally_acknowledged(document) {
                    resolve_recoverable_runtime_risks(document);
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

    #[cfg(test)]
    pub(crate) async fn record_edit_intent(&self, call_id: &str, cwd: &Path, paths: &[PathBuf]) {
        self.record_edit_intent_with_provenance(call_id, cwd, paths, None)
            .await;
    }

    pub(crate) async fn record_edit_intent_with_provenance(
        &self,
        call_id: &str,
        cwd: &Path,
        paths: &[PathBuf],
        provenance: Option<&ChildEvidenceProvenance>,
    ) {
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return;
        }
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
        let evidence_call_id = provenance
            .map(|value| format!("{}:{call_id}", value.source_thread_id))
            .unwrap_or_else(|| call_id.to_string());

        let Some((_, snapshot)) = self
            .update_document(|document| {
                document
                    .edit_intents
                    .retain(|intent| intent.call_id != evidence_call_id);
                document.edit_intents.push(EditIntent {
                    call_id: evidence_call_id,
                    step_id: document.active_step_id.clone(),
                    started_at: timestamp(),
                    completed_at: None,
                    outcome: None,
                    files,
                    source_thread_id: provenance.map(|value| value.source_thread_id.clone()),
                    source_agent_path: provenance.map(|value| value.source_agent_path.clone()),
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

    #[cfg(test)]
    pub(crate) async fn record_edit_result(&self, call_id: &str, outcome: &str) {
        self.record_edit_result_with_provenance(call_id, outcome, None)
            .await;
    }

    pub(crate) async fn record_edit_result_with_provenance(
        &self,
        call_id: &str,
        outcome: &str,
        provenance: Option<&ChildEvidenceProvenance>,
    ) {
        if self.mode == TaskEvidenceMode::EvidenceOnly {
            if outcome == "completed" {
                self.record_host_mutation().await;
            }
            return;
        }
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let evidence_call_id = provenance
            .map(|value| format!("{}:{call_id}", value.source_thread_id))
            .unwrap_or_else(|| call_id.to_string());
        let intent = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .and_then(|document| {
                    document
                        .edit_intents
                        .iter()
                        .find(|intent| intent.call_id == evidence_call_id)
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
            if before != &after || before.read_error.is_some() || after.read_error.is_some() {
                transitions.push(FileHashTransition {
                    path: before.path.clone(),
                    before_sha1: before.sha1.clone(),
                    after_sha1: after.sha1.clone(),
                    before_exists: before.exists,
                    after_exists: after.exists,
                    before_read_error: before.read_error.clone(),
                    after_read_error: after.read_error.clone(),
                });
            }
            after_snapshots.push(after);
        }
        let edit_succeeded = edit_outcome_succeeded(outcome);

        let Some((_, snapshot)) = self
            .update_document(|document| {
                if let Some(stored) = document
                    .edit_intents
                    .iter_mut()
                    .find(|stored| stored.call_id == evidence_call_id)
                {
                    stored.completed_at = Some(timestamp());
                    stored.outcome = Some(outcome.to_string());
                }
                if !transitions.is_empty() {
                    invalidate_for_mutation(document);
                    let epoch = document.evidence_epoch;
                    let mut affected_steps = BTreeMap::<String, BTreeSet<String>>::new();
                    for transition in &transitions {
                        if let Some(step_id) = intent.step_id.as_ref()
                            && document.plan.iter().any(|step| &step.id == step_id)
                        {
                            affected_steps
                                .entry(step_id.clone())
                                .or_default()
                                .insert(transition.path.clone());
                        }
                        for step in &document.plan {
                            if step.edit_paths.contains(&transition.path) {
                                affected_steps
                                    .entry(step.id.clone())
                                    .or_default()
                                    .insert(transition.path.clone());
                            }
                        }
                    }
                    for step in &mut document.plan {
                        if let Some(paths) = affected_steps.get(&step.id) {
                            step.edit_paths.extend(paths.iter().cloned());
                            if edit_succeeded
                                && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                            {
                                step.status = StepStatus::Implemented;
                            }
                        }
                    }
                    if affected_steps.is_empty() {
                        upsert_risk(
                            document,
                            EvidenceRisk {
                                id: format!("unassociated-edit-{evidence_call_id}"),
                                description: format!(
                                    "edit `{evidence_call_id}` changed files without an active plan step"
                                ),
                                source: "edit".to_string(),
                                blocking: false,
                                resolved: false,
                                epoch,
                            },
                        );
                    }
                    for after in after_snapshots {
                        if after.read_error.is_some() {
                            upsert_risk(document, unreadable_file_risk(&after.path, epoch, "edit"));
                        }
                        document
                            .latest_file_hashes
                            .insert(after.path.clone(), after);
                    }
                    let receipt_id =
                        next_receipt_id("edit", &mut document.next_edit_receipt_sequence);
                    document.edit_receipts.push(EditReceipt {
                        id: receipt_id,
                        call_id: evidence_call_id,
                        step_id: intent.step_id,
                        recorded_at: timestamp(),
                        epoch,
                        outcome: outcome.to_string(),
                        files: transitions,
                        source_thread_id: intent.source_thread_id,
                        source_agent_path: intent.source_agent_path,
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

    #[cfg(test)]
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
        self.record_command_with_provenance(
            command,
            cwd,
            exit_code,
            timed_out,
            duration_ms,
            possible_mutation,
            None,
        )
        .await;
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command_with_provenance(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
        provenance: Option<&ChildEvidenceProvenance>,
    ) {
        self.record_command_bound_with_provenance(
            command,
            cwd,
            exit_code,
            timed_out,
            duration_ms,
            possible_mutation,
            provenance,
            None,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_command_bound_with_provenance(
        &self,
        command: &[String],
        cwd: &PathUri,
        exit_code: i32,
        timed_out: bool,
        duration_ms: u64,
        possible_mutation: bool,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
    ) {
        if self.mode == TaskEvidenceMode::EvidenceOnly {
            if possible_mutation {
                self.record_host_mutation().await;
            }
            return;
        }
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return;
        }
        let command_succeeded = exit_code == 0 && !timed_out;
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
                        && command_succeeded
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
                let receipt_id = next_receipt_id(
                    "command",
                    &mut document.next_command_receipt_sequence,
                );
                let contract_binding = completion_contract_hashes(document, false);
                document.command_receipts.push(CommandReceipt {
                    id: receipt_id,
                    recorded_at: timestamp(),
                    epoch: document.evidence_epoch,
                    step_id: document.active_step_id.clone(),
                    command: command.to_vec(),
                    cwd: cwd.to_string(),
                    exit_code,
                    timed_out,
                    duration_ms,
                    possible_mutation,
                    host_mutation_revision: Some(document.host_mutation_revision),
                    manifest_revision: contract_binding
                        .as_ref()
                        .map(|(revision, _, _)| *revision),
                    user_source_ledger_hash: contract_binding
                        .as_ref()
                        .map(|(_, source_hash, _)| source_hash.clone()),
                    requirement_manifest_hash: contract_binding
                        .map(|(_, _, manifest_hash)| manifest_hash),
                    implementation_identity_hash: implementation_identity_hash.map(str::to_string),
                    source_thread_id: provenance.map(|value| value.source_thread_id.clone()),
                    source_agent_path: provenance.map(|value| value.source_agent_path.clone()),
                });
                trim_to_last(&mut document.command_receipts, MAX_COMMAND_RECEIPTS);
                document.updated_at = timestamp();
                document.completion = None;
            })
            .await
        else {
            return;
        };
        self.persist_document(&snapshot).await;
    }

    pub(crate) async fn finalization_advisory(&self) -> Option<String> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let gate = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            task_is_tracked(document)
                .then(|| derive_completion_gate(document, self.evidence_path.as_deref()))?
        };
        if gate.status == TaskCompletionStatus::Passed {
            return None;
        }
        let reasons = gate.reasons.iter().take(2).cloned().collect::<Vec<_>>();
        let reason_summary = if reasons.is_empty() {
            "evidence is incomplete".to_string()
        } else {
            reasons.join("; ")
        };
        let remaining = gate.reasons.len().saturating_sub(reasons.len());
        let remaining = if remaining == 0 {
            String::new()
        } else {
            format!("; and {remaining} more")
        };
        Some(format!(
            "KD4 task evidence is {status}: {reason_summary}{remaining}.",
            status = completion_status_name(gate.status),
        ))
    }

    pub(crate) async fn completion_review_dossier(
        &self,
        candidate_completion: Option<&str>,
        typed_mutation_identities: &[String],
        typed_evidence: &[String],
        authoritative_lens_facts: &ReviewLensSelectionFacts,
        authoritative_input_errors: &[String],
        typed_quiescent: bool,
        default_children_quiescent: bool,
    ) -> Option<CompletionReviewDossier> {
        if !self.allows_kd4_completion() {
            return None;
        }
        self.refresh_external_file_freshness().await;
        let source_capture_failed = self.user_source_capture_failed();
        let candidate_completion = candidate_completion.map(str::to_string);
        let mut authoritative_input_errors = authoritative_input_errors.to_vec();
        authoritative_input_errors.sort();
        authoritative_input_errors.dedup();
        let (
            document_revision,
            root_task_id,
            completion_epoch,
            manifest_revision,
            sources,
            source_mappings,
            source_classification_cache,
            source_classification_current,
            relationship_resolution_current,
            mappings_classified,
            requirements,
            user_source_ledger_hash,
            requirement_manifest_hash,
            implementation_identity_hash,
            dossier_snapshot_id,
            host_mutation_revision,
            has_task_attributed_mutations,
            evidence_gate,
            locally_obtainable_proof_routes,
            reviewer_visible_evidence,
            review_lens_selection_facts,
            correction_consumed,
            cycle_phase,
            active_cycle_id,
            cycle_parent_review_id,
            cycle_superseded_review_id,
            accepted_review_id,
            initial_review_id,
            initial_repair_instruction_hash,
            original_findings,
            manifest_gap_reconstructed,
            current_repair_snapshot,
            initial_repair_baseline,
            initial_repair_baseline_hash,
        ) = {
            let guard = self.document.lock().await;
            let document = guard.as_ref()?;
            let ledger = document.completion_review_v2.as_ref()?;
            let mut sources = ledger
                .source_records
                .values()
                .filter(|source| source.completion_epoch == ledger.completion_epoch)
                .cloned()
                .collect::<Vec<_>>();
            sources.sort_by_key(|source| (source.source_ordinal, source.content_ordinal));
            let source_mapping_revisions = source_mapping_revisions_for(
                ledger,
                ledger.completion_epoch,
                ledger.manifest_revision,
            );
            let source_mappings = source_mapping_revisions
                .iter()
                .map(|(source_id, revision)| (source_id.clone(), revision.mapping.clone()))
                .collect::<BTreeMap<_, _>>();
            let source_classification_current = sources.iter().all(|source| {
                source_mapping_revisions
                    .get(&source.source_id)
                    .is_some_and(|revision| {
                        revision.source_classification_contract_version.as_deref()
                            == Some(SOURCE_CLASSIFICATION_CONTRACT_VERSION)
                            && !matches!(revision.mapping, SourceMapping::PendingClassification)
                    })
            });
            let relationship_resolution_current = sources.iter().all(|source| {
                source_mapping_revisions
                    .get(&source.source_id)
                    .is_some_and(|revision| {
                        revision.relationship_resolver_contract_version.as_deref()
                            == Some(RELATIONSHIP_RESOLVER_CONTRACT_VERSION)
                            && !matches!(revision.mapping, SourceMapping::PendingClassification)
                    })
            });
            let mappings_classified =
                source_classification_current && relationship_resolution_current;
            let source_classification_cache = document
                .source_classification_cache
                .iter()
                .filter(|entry| entry.contract_version == SOURCE_CLASSIFICATION_CONTRACT_VERSION)
                .map(|entry| (entry.key(), entry.classification.clone()))
                .collect::<BTreeMap<_, _>>();
            let requirements = active_manifest(ledger)
                .map(|manifest| manifest.requirements.clone())
                .unwrap_or_default();
            let user_source_ledger_hash = user_source_ledger_snapshot_hash(
                ledger,
                ledger.completion_epoch,
                ledger.manifest_revision,
                source_capture_failed,
            );
            let requirement_manifest_hash =
                requirement_manifest_hash(ledger.manifest_revision, &requirements);
            let mut review_lens_selection_facts = authoritative_lens_facts.clone();
            review_lens_selection_facts.task_mutation_paths.extend(
                document
                    .latest_file_hashes
                    .values()
                    .map(|snapshot| snapshot.path.clone()),
            );
            review_lens_selection_facts.child_mutation_paths.extend(
                ledger
                    .attributed_workspace_events
                    .iter()
                    .flat_map(|event| event.paths.iter().cloned()),
            );
            review_lens_selection_facts.plan_edit_paths.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.edit_paths.iter().cloned()),
            );
            review_lens_selection_facts.plan_runtime_paths.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.runtime_paths.iter().cloned()),
            );
            review_lens_selection_facts.risk_hints.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.risks.iter().cloned()),
            );
            review_lens_selection_facts.generated_artifacts.extend(
                document
                    .plan
                    .iter()
                    .flat_map(|step| step.generated_artifacts.iter().cloned()),
            );
            review_lens_selection_facts.generated_artifacts.extend(
                document
                    .generated_artifact_requirements
                    .iter()
                    .filter_map(|artifact| artifact.path.clone()),
            );
            for values in [
                &mut review_lens_selection_facts.risk_hints,
                &mut review_lens_selection_facts.task_mutation_paths,
                &mut review_lens_selection_facts.child_mutation_paths,
                &mut review_lens_selection_facts.plan_edit_paths,
                &mut review_lens_selection_facts.plan_runtime_paths,
                &mut review_lens_selection_facts.surface_roles,
                &mut review_lens_selection_facts.validation_asset_paths,
                &mut review_lens_selection_facts.generated_artifacts,
            ] {
                values.sort();
                values.dedup();
            }
            let mut typed_mutation_identities = typed_mutation_identities.to_vec();
            typed_mutation_identities.sort();
            typed_mutation_identities.dedup();
            let mut path_hashes = document
                .latest_file_hashes
                .values()
                .map(|snapshot| {
                    serde_json::json!({
                        "kind": "task_attributed",
                        "path": normalize_path_for_identity(Path::new(&snapshot.path)),
                        "exists": snapshot.exists,
                        "contentHash": snapshot.sha1,
                        "readError": snapshot.read_error,
                    })
                })
                .chain(
                    document
                        .latest_generated_artifact_hashes
                        .values()
                        .map(|snapshot| {
                            serde_json::json!({
                                "kind": "generated_artifact",
                                "path": normalize_path_for_identity(Path::new(&snapshot.path)),
                                "exists": snapshot.exists,
                                "contentHash": snapshot.sha1,
                                "readError": snapshot.read_error,
                            })
                        }),
                )
                .collect::<Vec<_>>();
            path_hashes.sort_by_key(std::string::ToString::to_string);
            let mut default_child_mutation_identities = ledger
                .attributed_workspace_events
                .iter()
                .map(|event| {
                    serde_json::json!({
                        "workspaceId": event.workspace_id,
                        "epoch": event.epoch,
                        "actorId": event.actor_id,
                        "paths": event.paths,
                    })
                })
                .collect::<Vec<_>>();
            default_child_mutation_identities.sort_by_key(std::string::ToString::to_string);
            let has_task_attributed_mutations = document.host_mutation_revision > 0
                || !path_hashes.is_empty()
                || !default_child_mutation_identities.is_empty()
                || !typed_mutation_identities.is_empty();
            let implementation_identity_hash = canonical_hash(
                IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT,
                &serde_json::json!({
                    "rootTaskId": ledger.root_task_id,
                    "completionEpoch": ledger.completion_epoch,
                    "manifestRevision": ledger.manifest_revision,
                    "userSourceLedgerHash": user_source_ledger_hash,
                    "requirementManifestHash": requirement_manifest_hash,
                    "mutationRevision": document.host_mutation_revision,
                    "paths": path_hashes,
                    "defaultChildMutationIdentities": default_child_mutation_identities,
                    "typedMutationIdentities": typed_mutation_identities,
                }),
            );
            let mut proof_receipts = document
                .command_receipts
                .iter()
                .filter(|receipt| {
                    receipt.epoch == document.evidence_epoch
                        && receipt.host_mutation_revision == Some(document.host_mutation_revision)
                        && receipt.manifest_revision == Some(ledger.manifest_revision)
                        && receipt.user_source_ledger_hash.as_deref()
                            == Some(user_source_ledger_hash.as_str())
                        && receipt.requirement_manifest_hash.as_deref()
                            == Some(requirement_manifest_hash.as_str())
                        && receipt.implementation_identity_hash.as_deref()
                            == Some(implementation_identity_hash.as_str())
                })
                .map(|receipt| {
                    serde_json::json!({
                        "id": receipt.id,
                        "command": receipt.command,
                        "cwd": normalize_path_for_identity(Path::new(&receipt.cwd)),
                        "exitCode": receipt.exit_code,
                        "timedOut": receipt.timed_out,
                        "possibleMutation": receipt.possible_mutation,
                        "sourceThreadId": receipt.source_thread_id,
                        "sourceAgentPath": receipt.source_agent_path,
                        "boundImplementationIdentity": receipt.implementation_identity_hash,
                    })
                })
                .collect::<Vec<_>>();
            proof_receipts.sort_by_key(std::string::ToString::to_string);
            let mut external_evidence = document
                .external_evidence
                .iter()
                .filter(|receipt| {
                    receipt.task_epoch == document.evidence_epoch
                        && receipt.host_mutation_revision == Some(document.host_mutation_revision)
                        && receipt.implementation_identity_hash.as_deref()
                            == Some(implementation_identity_hash.as_str())
                })
                .map(|receipt| {
                    serde_json::json!({
                        "id": receipt.id,
                        "resultHash": receipt.result_sha256,
                        "complete": evidence_completeness_name(receipt.payload_completeness),
                        "truncated": receipt.truncated,
                        "approximate": receipt.approximate,
                        "limitations": receipt.limitations,
                        "providerSnapshot": receipt.provider_snapshot,
                        "sourceThreadId": receipt.source_thread_id,
                        "sourceAgentPath": receipt.source_agent_path,
                        "boundImplementationIdentity": receipt.implementation_identity_hash,
                    })
                })
                .collect::<Vec<_>>();
            external_evidence.sort_by_key(std::string::ToString::to_string);
            let mut typed_evidence = typed_evidence.to_vec();
            typed_evidence.sort();
            typed_evidence.dedup();
            let evidence_gate = derive_completion_gate(document, self.evidence_path.as_deref());
            let locally_obtainable_proof_routes =
                completion_review_locally_obtainable_proof_routes(&evidence_gate);
            let desktop_activation =
                document
                    .desktop_activation_receipt
                    .as_ref()
                    .filter(|receipt| {
                        desktop_activation_receipt_is_complete(receipt)
                            && receipt.implementation_identity_hash.as_deref()
                                == Some(implementation_identity_hash.as_str())
                    });
            let reviewer_visible_evidence = serde_json::json!({
                "implementationIdentity": implementation_identity_hash,
                "taskAttributedPaths": path_hashes,
                "defaultChildMutationIdentities": default_child_mutation_identities,
                "typedMutationIdentities": typed_mutation_identities,
                "proofReceipts": proof_receipts,
                "externalEvidence": external_evidence,
                "desktopActivation": desktop_activation,
                "plan": document.plan,
                "risks": document.risks,
                "evidenceGate": evidence_gate,
                "locallyObtainableProofRoutes": locally_obtainable_proof_routes,
                "typedEvidence": typed_evidence,
                "authoritativeInputErrors": authoritative_input_errors,
                "typedQuiescent": typed_quiescent,
                "defaultChildrenQuiescent": default_children_quiescent,
                "candidateCompletion": candidate_completion,
            });
            let dossier_snapshot_id = canonical_hash(
                DOSSIER_SNAPSHOT_CANONICAL_FORMAT,
                &reviewer_visible_evidence,
            );
            let cycle = ledger.active_review_cycle.as_ref();
            let current_repair_snapshot =
                current_repair_snapshot(document, &typed_mutation_identities);
            let initial_receipt = cycle.and_then(|cycle| {
                if matches!(
                    cycle.phase,
                    CompletionReviewCyclePhase::ClassificationPending
                        | CompletionReviewCyclePhase::InitialReviewPending
                ) {
                    return None;
                }
                let cycle_start = cycle
                    .parent_terminal_review_id
                    .as_ref()
                    .and_then(|parent_id| {
                        ledger
                            .receipts
                            .iter()
                            .position(|receipt| receipt.review_id == *parent_id)
                    })
                    .map_or(0, |parent_index| parent_index.saturating_add(1));
                ledger.receipts[cycle_start..].iter().rev().find(|receipt| {
                    receipt.attempt_kind == CompletionReviewAttemptKind::InitialReview
                        && receipt.requirement_manifest_hash == requirement_manifest_hash
                        && receipt.terminal_outcome.is_none()
                })
            });
            (
                document.revision,
                ledger.root_task_id.clone(),
                ledger.completion_epoch,
                ledger.manifest_revision,
                sources,
                source_mappings,
                source_classification_cache,
                source_classification_current,
                relationship_resolution_current,
                mappings_classified,
                requirements,
                user_source_ledger_hash,
                requirement_manifest_hash,
                implementation_identity_hash,
                dossier_snapshot_id,
                document.host_mutation_revision,
                has_task_attributed_mutations,
                evidence_gate,
                locally_obtainable_proof_routes,
                reviewer_visible_evidence,
                review_lens_selection_facts,
                cycle.is_some_and(|cycle| cycle.correction_consumed),
                cycle.map(|cycle| cycle.phase),
                cycle.map(|cycle| cycle.cycle_id.clone()),
                cycle.and_then(|cycle| cycle.parent_terminal_review_id.clone()),
                cycle.and_then(|cycle| cycle.superseded_review_id.clone()),
                cycle.and_then(|cycle| cycle.accepted_review_id.clone()),
                initial_receipt.map(|receipt| receipt.review_id.clone()),
                initial_receipt.and_then(|receipt| receipt.repair_instruction_hash.clone()),
                initial_receipt
                    .map(|receipt| receipt.findings.clone())
                    .unwrap_or_default(),
                cycle.is_some_and(|cycle| cycle.manifest_gap_reconstructed),
                current_repair_snapshot,
                initial_receipt.and_then(|receipt| receipt.repair_baseline.clone()),
                initial_receipt.and_then(|receipt| receipt.baseline_hash.clone()),
            )
        };
        let rereview_input = build_rereview_input(
            initial_repair_baseline.as_ref(),
            initial_repair_baseline_hash.as_deref(),
            initial_repair_instruction_hash.as_deref(),
            &original_findings,
            &current_repair_snapshot,
            &implementation_identity_hash,
            &user_source_ledger_hash,
            &requirement_manifest_hash,
        );
        Some(CompletionReviewDossier {
            document_revision,
            root_task_id,
            completion_epoch,
            manifest_revision,
            sources,
            source_mappings,
            source_classification_cache,
            source_classification_current,
            relationship_resolution_current,
            mappings_classified,
            source_capture_failed,
            requirements,
            user_source_ledger_hash,
            requirement_manifest_hash,
            implementation_identity_hash,
            dossier_snapshot_id,
            host_mutation_revision,
            has_task_attributed_mutations,
            evidence_gate,
            locally_obtainable_proof_routes,
            reviewer_visible_evidence,
            review_lens_selection_facts,
            authoritative_input_errors,
            typed_quiescent,
            default_children_quiescent,
            candidate_completion,
            correction_consumed,
            cycle_phase,
            active_cycle_id,
            cycle_parent_review_id,
            cycle_superseded_review_id,
            accepted_review_id,
            initial_review_id,
            initial_repair_instruction_hash,
            original_findings,
            manifest_gap_reconstructed,
            current_repair_snapshot,
            initial_repair_baseline,
            initial_repair_baseline_hash,
            rereview_input,
        })
    }

    pub(crate) async fn begin_completion_review_cycle(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<String> {
        self.atomic_review_update(dossier.document_revision, None, None, |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                unreachable!("V2 dossier requires a V2 ledger");
            };
            let cycle = ledger
                .active_review_cycle
                .get_or_insert_with(|| CompletionReviewCycle {
                    cycle_id: format!(
                        "cycle-{}-{}",
                        ledger.completion_epoch, ledger.manifest_revision
                    ),
                    manifest_revision: ledger.manifest_revision,
                    parent_terminal_review_id: ledger.last_terminal_closure.clone(),
                    superseded_review_id: None,
                    phase: if dossier.mappings_classified {
                        CompletionReviewCyclePhase::InitialReviewPending
                    } else {
                        CompletionReviewCyclePhase::ClassificationPending
                    },
                    correction_consumed: false,
                    manifest_gap_reconstructed: false,
                    accepted_review_id: None,
                    accepted_dossier_snapshot_id: None,
                });
            if cycle.manifest_revision != dossier.manifest_revision {
                cycle.manifest_revision = dossier.manifest_revision;
                cycle.phase = if dossier.mappings_classified {
                    CompletionReviewCyclePhase::InitialReviewPending
                } else {
                    CompletionReviewCyclePhase::ClassificationPending
                };
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
                cycle.manifest_gap_reconstructed = false;
                cycle.superseded_review_id = None;
            }
            ledger.review_risk = CompletionReviewRisk {
                unresolved: true,
                cycle_id: Some(cycle.cycle_id.clone()),
                opened_at: Some(timestamp()),
                resolved_at: None,
            };
            document.completion = None;
            document.updated_at = timestamp();
            cycle.cycle_id.clone()
        })
        .await
    }

    pub(crate) async fn preview_completion_review_id(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> Option<String> {
        let guard = self.document.lock().await;
        let document = guard.as_ref()?;
        if document.revision != dossier.document_revision {
            return None;
        }
        let ledger = document.completion_review_v2.as_ref()?;
        Some(format!(
            "review-{}-{}-{}",
            ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
        ))
    }

    pub(crate) async fn record_completion_review_attempt_v2(
        &self,
        dossier: &CompletionReviewDossier,
        input: CompletionReviewAttemptInput,
    ) -> AtomicReviewTransition<RecordedReviewAttempt> {
        self.record_completion_review_attempt_v2_inner(dossier, input, None)
            .await
    }

    pub(crate) async fn record_completion_review_attempt_v2_with_materialization(
        &self,
        dossier: &CompletionReviewDossier,
        input: CompletionReviewAttemptInput,
        materialization: SourceMaterialization,
    ) -> AtomicReviewTransition<RecordedReviewAttempt> {
        self.record_completion_review_attempt_v2_inner(dossier, input, Some(materialization))
            .await
    }

    async fn record_completion_review_attempt_v2_inner(
        &self,
        dossier: &CompletionReviewDossier,
        input: CompletionReviewAttemptInput,
        gap_materialization: Option<SourceMaterialization>,
    ) -> AtomicReviewTransition<RecordedReviewAttempt> {
        if input.attempt_kind == CompletionReviewAttemptKind::TerminalClosure {
            return AtomicReviewTransition::Failed;
        }
        let reconstruct_manifest = !input.manifest_gaps.is_empty();
        let prepared_gap_materialization = match (reconstruct_manifest, gap_materialization) {
            (true, Some(materialization)) => {
                let Some(prepared) = prepare_source_materialization(dossier, materialization)
                else {
                    return AtomicReviewTransition::Failed;
                };
                if !prepared_materialization_covers_manifest_gaps(
                    dossier,
                    &input.manifest_gaps,
                    &prepared,
                ) {
                    return AtomicReviewTransition::Failed;
                }
                Some(prepared)
            }
            (false, None) => None,
            _ => return AtomicReviewTransition::Failed,
        };
        let expected_ordinals = (1..=input.findings.len() as u32).collect::<Vec<_>>();
        let ordinals = input
            .findings
            .iter()
            .map(|finding| finding.local_ordinal)
            .collect::<Vec<_>>();
        if ordinals != expected_ordinals
            || (input.review_clean && !input.findings.is_empty())
            || (input.repair_instruction.is_some() && input.repair_instruction_hash.is_some())
            || input
                .repair_instruction_hash
                .as_deref()
                .is_some_and(|hash| {
                    hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            || (reconstruct_manifest
                && (input.review_clean
                    || input.repair_instruction.is_some()
                    || input.infrastructure_outcome != "ok"
                    || input.terminal_outcome.is_some()))
            || input.infrastructure_outcome.trim().is_empty()
            || !matches!(
                input.terminal_outcome.as_deref(),
                None | Some("partial") | Some("blocked")
            )
        {
            return AtomicReviewTransition::Failed;
        }
        let infrastructure_ok = input.infrastructure_outcome == "ok";
        if !infrastructure_ok
            && (input.review_clean
                || !input.findings.is_empty()
                || !input.dispositions.is_empty()
                || !input.manifest_gaps.is_empty()
                || input.terminal_outcome.as_deref() != Some("partial"))
        {
            return AtomicReviewTransition::Failed;
        }
        let valid_requirement_ids = dossier
            .requirements
            .iter()
            .map(|requirement| requirement.requirement_id.as_str())
            .collect::<BTreeSet<_>>();
        let active_requirement_ids = dossier
            .requirements
            .iter()
            .filter(|requirement| requirement.status == RequirementStatus::Active)
            .map(|requirement| requirement.requirement_id.as_str())
            .collect::<BTreeSet<_>>();
        if input.findings.iter().any(|finding| {
            let unique_requirement_ids = finding
                .requirement_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            unique_requirement_ids.len() != finding.requirement_ids.len()
                || (!unique_requirement_ids.is_empty()
                    && !unique_requirement_ids
                        .iter()
                        .any(|requirement_id| active_requirement_ids.contains(requirement_id)))
                || finding
                    .requirement_ids
                    .iter()
                    .any(|requirement_id| !valid_requirement_ids.contains(requirement_id.as_str()))
                || finding.lens.trim().is_empty()
                || finding.contract_surface.trim().is_empty()
                || finding.severity.trim().is_empty()
                || finding.evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.proof_route.trim().is_empty()
        }) {
            return AtomicReviewTransition::Failed;
        }
        let valid_dispositions = [
            "resolved",
            "rebuttal_accepted",
            "still_present",
            "insufficient_proof",
            "regressed",
        ];
        if input.dispositions.iter().any(|disposition| {
            disposition.finding_id.trim().is_empty()
                || disposition.evidence.trim().is_empty()
                || !valid_dispositions.contains(&disposition.disposition.as_str())
        }) {
            return AtomicReviewTransition::Failed;
        }
        let needs_correction = input.repair_instruction.is_some()
            || (input.attempt_kind == CompletionReviewAttemptKind::CorrectionEvidence
                && input.repair_instruction_hash.is_some());
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(cycle) = ledger.active_review_cycle.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision
                || cycle.manifest_revision != dossier.manifest_revision
            {
                return AtomicReviewTransition::Failed;
            }
            let valid_lifecycle = match input.attempt_kind {
                CompletionReviewAttemptKind::InitialReview => {
                    (cycle.phase == CompletionReviewCyclePhase::InitialReviewPending
                        || (!infrastructure_ok
                            && cycle.phase == CompletionReviewCyclePhase::ClassificationPending))
                        && input.parent_review_id == cycle.parent_terminal_review_id
                        && input.superseded_review_id == cycle.superseded_review_id
                        && input.dispositions.is_empty()
                        && input.repair_instruction_hash.is_none()
                }
                CompletionReviewAttemptKind::CorrectionEvidence => {
                    cycle.phase == CompletionReviewCyclePhase::CorrectionPending
                        && !cycle.correction_consumed
                        && input.superseded_review_id.is_none()
                        && input.parent_review_id == dossier.initial_review_id
                        && input.findings.is_empty()
                        && input.dispositions.is_empty()
                        && needs_correction
                        && input.repair_instruction.is_none()
                        && input.repair_instruction_hash == dossier.initial_repair_instruction_hash
                        && !input.review_clean
                }
                CompletionReviewAttemptKind::Rereview => {
                    cycle.phase == CompletionReviewCyclePhase::RereviewPending
                        && cycle.correction_consumed
                        && input.superseded_review_id.is_none()
                        && input.parent_review_id == dossier.initial_review_id
                        && input.repair_instruction_hash == dossier.initial_repair_instruction_hash
                        && input.repair_instruction.is_none()
                }
                CompletionReviewAttemptKind::TerminalClosure => false,
            };
            if !valid_lifecycle {
                return AtomicReviewTransition::Failed;
            }
            if input.attempt_kind == CompletionReviewAttemptKind::Rereview && infrastructure_ok {
                let expected = dossier
                    .original_findings
                    .iter()
                    .map(|finding| finding.finding_id.as_str())
                    .collect::<BTreeSet<_>>();
                let returned = input
                    .dispositions
                    .iter()
                    .map(|disposition| disposition.finding_id.as_str())
                    .collect::<BTreeSet<_>>();
                if returned.len() != input.dispositions.len() || returned != expected {
                    return AtomicReviewTransition::Failed;
                }
            }
        }
        if let Some(parent_review_id) = input.parent_review_id.as_deref() {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision
                || document.completion_review_v2.as_ref().is_none_or(|ledger| {
                    !ledger
                        .receipts
                        .iter()
                        .any(|receipt| receipt.review_id == parent_review_id)
                })
            {
                return AtomicReviewTransition::Failed;
            }
        }
        let repair_instruction_hash = input.repair_instruction.as_deref().map_or_else(
            || input.repair_instruction_hash.clone(),
            |instruction| {
                Some(canonical_hash(
                    REPAIR_INSTRUCTION_CANONICAL_FORMAT,
                    &serde_json::json!({ "instruction": instruction }),
                ))
            },
        );
        let repair_baseline_metadata = if input.attempt_kind
            == CompletionReviewAttemptKind::InitialReview
            && input.repair_instruction.is_some()
        {
            let preview_findings = input
                .findings
                .iter()
                .map(|finding| CompletionReviewFindingReceipt {
                    finding_id: format!("preview/F{}", finding.local_ordinal),
                    requirement_ids: finding.requirement_ids.clone(),
                    lens: finding.lens.clone(),
                    contract_surface: finding.contract_surface.clone(),
                    severity: finding.severity.clone(),
                    evidence: finding.evidence.clone(),
                    smallest_correction: finding.smallest_correction.clone(),
                    proof_route: finding.proof_route.clone(),
                })
                .collect::<Vec<_>>();
            let Ok(baseline) = build_repair_baseline(dossier, &preview_findings) else {
                return AtomicReviewTransition::Failed;
            };
            let baseline_hash = repair_baseline_hash(&baseline);
            if !input
                .repair_instruction
                .as_deref()
                .is_some_and(|instruction| {
                    repair_instruction_matches_baseline(instruction, &baseline, &baseline_hash)
                })
            {
                return AtomicReviewTransition::Failed;
            }
            Some((baseline, baseline_hash))
        } else {
            None
        };
        let rereview_metadata = if input.attempt_kind == CompletionReviewAttemptKind::Rereview {
            let Some(rereview_input) = dossier.rereview_input.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if !validate_rereview_input(rereview_input, dossier) {
                return AtomicReviewTransition::Failed;
            }
            Some(rereview_input.clone())
        } else {
            None
        };
        let persisted_repair_baseline = repair_baseline_metadata
            .as_ref()
            .map(|(baseline, _)| baseline.clone());
        let persisted_baseline_hash = rereview_metadata
            .as_ref()
            .and_then(|metadata| metadata.baseline_hash.clone())
            .or_else(|| {
                repair_baseline_metadata
                    .as_ref()
                    .map(|(_, hash)| hash.clone())
            });
        let persisted_input_mode = rereview_metadata
            .as_ref()
            .map(|metadata| metadata.input_mode);
        let persisted_delta_hash = rereview_metadata
            .as_ref()
            .and_then(|metadata| metadata.delta_hash.clone());
        let persisted_rereview_delta = rereview_metadata
            .as_ref()
            .and_then(|metadata| metadata.delta.clone());
        let persisted_fallback_reasons = rereview_metadata
            .as_ref()
            .map(|metadata| metadata.fallback_reasons.clone())
            .unwrap_or_default();
        let persisted_candidate_identity = rereview_metadata
            .as_ref()
            .map(|metadata| metadata.candidate_implementation_identity.clone());
        let persisted_rereview_audit_hash = rereview_metadata.as_ref().map(rereview_audit_hash);
        let attempt_kind = input.attempt_kind;
        let parent_review_id = input.parent_review_id.clone();
        let superseded_review_id = input.superseded_review_id.clone();
        let dispositions = input.dispositions.clone();
        let manifest_gaps = input.manifest_gaps.clone();
        let infrastructure_outcome = input.infrastructure_outcome.clone();
        let terminal_outcome = input.terminal_outcome.clone();
        let review_clean = input.review_clean;
        let evidence_path = self
            .evidence_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision {
                return AtomicReviewTransition::Superseded;
            }
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if !dossier_sources_are_current(ledger, &dossier.sources) {
                return AtomicReviewTransition::Failed;
            }
        }
        let mut gap_persistence = prepared_gap_materialization.map(|prepared| {
            let PreparedSourceMaterialization {
                local_classifications,
                requirements,
                mappings,
            } = prepared;
            let replacement_cache_keys = local_classifications
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            let replacement_cache_entries = local_classifications
                .into_iter()
                .map(|(key, classification)| SourceClassificationCacheEntry {
                    contract_version: key.contract_version,
                    source_kind: key.source_kind,
                    content_hash: key.content_hash,
                    classification,
                })
                .collect::<Vec<_>>();
            (
                replacement_cache_keys,
                replacement_cache_entries,
                requirements,
                mappings,
            )
        });
        self.atomic_review_update(dossier.document_revision, None, None, move |document| {
            if let Some((replacement_cache_keys, replacement_cache_entries, _, _)) =
                gap_persistence.as_mut()
            {
                document
                    .source_classification_cache
                    .retain(|entry| !replacement_cache_keys.contains(&entry.key()));
                document
                    .source_classification_cache
                    .append(replacement_cache_entries);
            }
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                unreachable!("V2 dossier requires a V2 ledger");
            };
            let review_id = format!(
                "review-{}-{}-{}",
                ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
            );
            ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
            let findings = input
                .findings
                .into_iter()
                .map(|finding| CompletionReviewFindingReceipt {
                    finding_id: format!("{review_id}/F{}", finding.local_ordinal),
                    requirement_ids: finding.requirement_ids,
                    lens: finding.lens,
                    contract_surface: finding.contract_surface,
                    severity: finding.severity,
                    evidence: finding.evidence,
                    smallest_correction: finding.smallest_correction,
                    proof_route: finding.proof_route,
                })
                .collect::<Vec<_>>();
            ledger.receipts.push(CompletionReviewReceiptV2 {
                review_id: review_id.clone(),
                attempt_kind,
                parent_review_id: parent_review_id.clone(),
                superseded_review_id,
                candidate_mutation_revision: dossier.host_mutation_revision,
                candidate_hash: dossier.implementation_identity_hash.clone(),
                implementation_identity_hash: dossier.implementation_identity_hash.clone(),
                dossier_snapshot_id: dossier.dossier_snapshot_id.clone(),
                user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                findings: findings.clone(),
                dispositions,
                manifest_gaps: manifest_gaps.clone(),
                repair_instruction_hash,
                repair_baseline: persisted_repair_baseline,
                baseline_hash: persisted_baseline_hash,
                input_mode: persisted_input_mode,
                delta_hash: persisted_delta_hash,
                rereview_delta: persisted_rereview_delta,
                fallback_reasons: persisted_fallback_reasons,
                candidate_implementation_identity: persisted_candidate_identity,
                rereview_audit_hash: persisted_rereview_audit_hash,
                infrastructure_outcome,
                review_clean,
                terminal_outcome: None,
                recorded_at: timestamp(),
            });
            let terminal_closure_id = terminal_outcome.as_deref().map(|outcome| {
                let terminal_review_id = format!(
                    "review-{}-{}-{}",
                    ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
                );
                ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
                ledger.receipts.push(CompletionReviewReceiptV2 {
                    review_id: terminal_review_id.clone(),
                    attempt_kind: CompletionReviewAttemptKind::TerminalClosure,
                    parent_review_id: Some(review_id.clone()),
                    superseded_review_id: None,
                    candidate_mutation_revision: dossier.host_mutation_revision,
                    candidate_hash: dossier.implementation_identity_hash.clone(),
                    implementation_identity_hash: dossier.implementation_identity_hash.clone(),
                    dossier_snapshot_id: dossier.dossier_snapshot_id.clone(),
                    user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                    requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                    findings: Vec::new(),
                    dispositions: Vec::new(),
                    manifest_gaps: Vec::new(),
                    repair_instruction_hash: None,
                    repair_baseline: None,
                    baseline_hash: None,
                    input_mode: None,
                    delta_hash: None,
                    rereview_delta: None,
                    fallback_reasons: Vec::new(),
                    candidate_implementation_identity: None,
                    rereview_audit_hash: None,
                    infrastructure_outcome: "ok".to_string(),
                    review_clean: false,
                    terminal_outcome: Some(outcome.to_string()),
                    recorded_at: timestamp(),
                });
                terminal_review_id
            });
            if reconstruct_manifest {
                let Some((_, _, requirements, mappings)) = gap_persistence.take() else {
                    unreachable!("manifest-gap reconstruction requires a prepared materialization");
                };
                let new_revision = ledger.manifest_revision.saturating_add(1);
                for (source_id, mapping) in mappings {
                    ledger.mapping_revisions.push(SourceMappingRevision {
                        completion_epoch: ledger.completion_epoch,
                        manifest_revision: new_revision,
                        source_id,
                        source_classification_contract_version: Some(
                            SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
                        ),
                        relationship_resolver_contract_version: Some(
                            RELATIONSHIP_RESOLVER_CONTRACT_VERSION.to_string(),
                        ),
                        mapping,
                    });
                }
                let manifest_hash = requirement_manifest_hash(new_revision, &requirements);
                let correction_consumed = ledger
                    .active_review_cycle
                    .as_ref()
                    .is_some_and(|cycle| cycle.correction_consumed);
                let parent_terminal_review_id = ledger
                    .active_review_cycle
                    .as_ref()
                    .and_then(|cycle| cycle.parent_terminal_review_id.clone());
                ledger.manifest_revision = new_revision;
                ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                    completion_epoch: ledger.completion_epoch,
                    manifest_revision: new_revision,
                    manifest_hash,
                    requirements,
                });
                ledger.active_review_cycle = Some(CompletionReviewCycle {
                    cycle_id: format!("cycle-{}-{new_revision}", ledger.completion_epoch),
                    manifest_revision: new_revision,
                    parent_terminal_review_id,
                    superseded_review_id: Some(review_id.clone()),
                    phase: CompletionReviewCyclePhase::InitialReviewPending,
                    correction_consumed,
                    manifest_gap_reconstructed: true,
                    accepted_review_id: None,
                    accepted_dossier_snapshot_id: None,
                });
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = ledger
                    .active_review_cycle
                    .as_ref()
                    .map(|cycle| cycle.cycle_id.clone());
                ledger.review_risk.opened_at = Some(timestamp());
                ledger.review_risk.resolved_at = None;
                document.evidence_epoch = document.evidence_epoch.saturating_add(1);
                document.desktop_activation_receipt = None;
            } else if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                if terminal_outcome.as_deref() == Some("partial") {
                    cycle.phase = CompletionReviewCyclePhase::TerminalPartial;
                } else if terminal_outcome.as_deref() == Some("blocked") {
                    cycle.phase = CompletionReviewCyclePhase::TerminalBlocked;
                } else {
                    match attempt_kind {
                        CompletionReviewAttemptKind::InitialReview => {
                            cycle.phase = if (needs_correction || !findings.is_empty())
                                && cycle.correction_consumed
                            {
                                CompletionReviewCyclePhase::TerminalPartial
                            } else if needs_correction {
                                CompletionReviewCyclePhase::CorrectionPending
                            } else if review_clean {
                                CompletionReviewCyclePhase::ProvisionalClean
                            } else if findings.is_empty() {
                                CompletionReviewCyclePhase::TerminalPartial
                            } else {
                                CompletionReviewCyclePhase::CorrectionPending
                            };
                        }
                        CompletionReviewAttemptKind::CorrectionEvidence => {
                            cycle.correction_consumed = true;
                            cycle.phase = CompletionReviewCyclePhase::RereviewPending;
                        }
                        CompletionReviewAttemptKind::Rereview => {
                            cycle.phase = if review_clean {
                                CompletionReviewCyclePhase::ProvisionalClean
                            } else {
                                CompletionReviewCyclePhase::TerminalPartial
                            };
                        }
                        CompletionReviewAttemptKind::TerminalClosure => unreachable!(),
                    }
                }
                if review_clean && !needs_correction && terminal_outcome.is_none() {
                    cycle.accepted_review_id = Some(review_id.clone());
                    cycle.accepted_dossier_snapshot_id = Some(dossier.dossier_snapshot_id.clone());
                }
            }
            if let Some(terminal_closure_id) = terminal_closure_id {
                ledger.last_terminal_closure = Some(terminal_closure_id);
                let status = if terminal_outcome.as_deref() == Some("blocked") {
                    TaskCompletionStatus::Blocked
                } else {
                    TaskCompletionStatus::Partial
                };
                let mut reasons = dossier.evidence_gate.reasons.clone();
                if reasons.is_empty() {
                    reasons.push(format!(
                        "completion review ended {outcome}; see review {review_id}",
                        outcome = terminal_outcome.as_deref().unwrap_or("partial")
                    ));
                }
                document.completion = Some(TaskCompletionGate {
                    status,
                    reasons,
                    evidence_path,
                });
            } else {
                document.completion = None;
            }
            document.updated_at = timestamp();
            RecordedReviewAttempt {
                review_id,
                findings,
            }
        })
        .await
    }

    pub(crate) async fn finalize_completion_review(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<TaskCompletionGate> {
        if self.user_source_capture_failed()
            || !dossier.mappings_classified
            || !dossier.typed_quiescent
            || !dossier.default_children_quiescent
            || dossier.evidence_gate.status != TaskCompletionStatus::Passed
            || dossier.sources.iter().any(|source| {
                source.availability != UserSourceAvailability::Available
                    || matches!(
                        dossier.source_mappings.get(&source.source_id),
                        Some(
                            SourceMapping::PendingClassification
                                | SourceMapping::UnavailableOrTruncated
                        )
                    )
            })
        {
            return AtomicReviewTransition::Failed;
        }
        let Some(accepted_review_id) = dossier.accepted_review_id.clone() else {
            return AtomicReviewTransition::Failed;
        };
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(cycle) = ledger.active_review_cycle.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            let Some(accepted) = ledger
                .receipts
                .iter()
                .find(|receipt| receipt.review_id == accepted_review_id)
            else {
                return AtomicReviewTransition::Failed;
            };
            let activation_required = document
                .plan
                .iter()
                .any(|step| step.requires_desktop_activation);
            let activation_current = !activation_required
                || document
                    .desktop_activation_receipt
                    .as_ref()
                    .is_some_and(|receipt| {
                        desktop_activation_receipt_is_complete(receipt)
                            && receipt.epoch == document.evidence_epoch
                            && receipt.implementation_identity_hash.as_deref()
                                == Some(dossier.implementation_identity_hash.as_str())
                    });
            if document.revision != dossier.document_revision
                || !ledger.review_risk.unresolved
                || cycle.phase != CompletionReviewCyclePhase::ProvisionalClean
                || cycle.accepted_review_id.as_deref() != Some(accepted_review_id.as_str())
                || cycle.accepted_dossier_snapshot_id.as_deref()
                    != Some(dossier.dossier_snapshot_id.as_str())
                || !accepted.review_clean
                || accepted.terminal_outcome.is_some()
                || accepted.implementation_identity_hash != dossier.implementation_identity_hash
                || accepted.dossier_snapshot_id != dossier.dossier_snapshot_id
                || !activation_current
            {
                return AtomicReviewTransition::Superseded;
            }
        }

        let accepted_parent = accepted_review_id.clone();
        let completion = TaskCompletionGate {
            status: TaskCompletionStatus::Passed,
            reasons: Vec::new(),
            evidence_path: self
                .evidence_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        let transition = self
            .atomic_review_update(
                dossier.document_revision,
                Some(&dossier.implementation_identity_hash),
                Some(&dossier.dossier_snapshot_id),
                |document| {
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        unreachable!("V2 dossier requires a V2 ledger");
                    };
                    let review_id = format!(
                        "review-{}-{}-{}",
                        ledger.completion_epoch,
                        ledger.manifest_revision,
                        ledger.next_review_sequence
                    );
                    ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
                    ledger.receipts.push(CompletionReviewReceiptV2 {
                        review_id: review_id.clone(),
                        attempt_kind: CompletionReviewAttemptKind::TerminalClosure,
                        parent_review_id: Some(accepted_parent),
                        superseded_review_id: None,
                        candidate_mutation_revision: dossier.host_mutation_revision,
                        candidate_hash: dossier.implementation_identity_hash.clone(),
                        implementation_identity_hash: dossier.implementation_identity_hash.clone(),
                        dossier_snapshot_id: dossier.dossier_snapshot_id.clone(),
                        user_source_ledger_hash: dossier.user_source_ledger_hash.clone(),
                        requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
                        findings: Vec::new(),
                        dispositions: Vec::new(),
                        manifest_gaps: Vec::new(),
                        repair_instruction_hash: None,
                        repair_baseline: None,
                        baseline_hash: None,
                        input_mode: None,
                        delta_hash: None,
                        rereview_delta: None,
                        fallback_reasons: Vec::new(),
                        candidate_implementation_identity: None,
                        rereview_audit_hash: None,
                        infrastructure_outcome: "ok".to_string(),
                        review_clean: true,
                        terminal_outcome: Some("passed".to_string()),
                        recorded_at: timestamp(),
                    });
                    if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                        cycle.phase = CompletionReviewCyclePhase::Closed;
                    }
                    ledger.review_risk.unresolved = false;
                    ledger.review_risk.resolved_at = Some(timestamp());
                    ledger.last_terminal_closure = Some(review_id);
                    document.completion = Some(completion.clone());
                    document.updated_at = timestamp();
                    completion.clone()
                },
            )
            .await;
        if matches!(&transition, AtomicReviewTransition::Persisted(_)) {
            self.source_capture_failed.store(false, Ordering::Release);
        }
        transition
    }

    pub(crate) async fn passed_completion_matches_dossier(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> bool {
        if dossier.evidence_gate.status != TaskCompletionStatus::Passed
            || !dossier.typed_quiescent
            || !dossier.default_children_quiescent
            || !dossier.authoritative_input_errors.is_empty()
        {
            return false;
        }
        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return false;
        };
        let Some(ledger) = document.completion_review_v2.as_ref() else {
            return false;
        };
        let Some(cycle) = ledger.active_review_cycle.as_ref() else {
            return false;
        };
        let Some(accepted_review_id) = cycle.accepted_review_id.as_deref() else {
            return false;
        };
        let Some(terminal_id) = ledger.last_terminal_closure.as_deref() else {
            return false;
        };
        let Some(terminal) = ledger
            .receipts
            .iter()
            .find(|receipt| receipt.review_id == terminal_id)
        else {
            return false;
        };
        document.revision == dossier.document_revision
            && document
                .completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
            && cycle.phase == CompletionReviewCyclePhase::Closed
            && !ledger.review_risk.unresolved
            && cycle.accepted_dossier_snapshot_id.as_deref()
                == Some(dossier.dossier_snapshot_id.as_str())
            && terminal.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
            && terminal.terminal_outcome.as_deref() == Some("passed")
            && terminal.parent_review_id.as_deref() == Some(accepted_review_id)
            && terminal.implementation_identity_hash == dossier.implementation_identity_hash
            && terminal.dossier_snapshot_id == dossier.dossier_snapshot_id
            && terminal.user_source_ledger_hash == dossier.user_source_ledger_hash
            && terminal.requirement_manifest_hash == dossier.requirement_manifest_hash
    }

    pub(crate) async fn supersede_provisional_completion_review(
        &self,
        dossier: &CompletionReviewDossier,
    ) -> AtomicReviewTransition<()> {
        self.atomic_review_update(dossier.document_revision, None, None, |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            if let Some(cycle) = ledger.active_review_cycle.as_mut()
                && cycle.phase == CompletionReviewCyclePhase::ProvisionalClean
            {
                cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = Some(cycle.cycle_id.clone());
                ledger.review_risk.opened_at = Some(timestamp());
                ledger.review_risk.resolved_at = None;
            }
            document.completion = None;
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn completion_review_correction_consumed(&self) -> bool {
        self.document
            .lock()
            .await
            .as_ref()
            .and_then(|document| document.completion_review_v2.as_ref())
            .and_then(|ledger| ledger.active_review_cycle.as_ref())
            .is_some_and(|cycle| cycle.correction_consumed)
    }

    pub(crate) async fn prepare_after_agent_completion_review_reentry(
        &self,
        preserve_correction_consumed: bool,
    ) -> AtomicReviewTransition<()> {
        let Some(expected_revision) = self.document_revision().await else {
            return AtomicReviewTransition::Failed;
        };
        self.atomic_review_update(expected_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            let parent_terminal_review_id = ledger
                .receipts
                .iter()
                .rev()
                .find(|receipt| {
                    receipt.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
                })
                .map(|receipt| receipt.review_id.clone());
            let prior_consumed = ledger
                .active_review_cycle
                .as_ref()
                .is_some_and(|cycle| cycle.correction_consumed);
            let correction_consumed = prior_consumed || preserve_correction_consumed;
            let manifest_gap_reconstructed = ledger
                .active_review_cycle
                .as_ref()
                .is_some_and(|cycle| cycle.manifest_gap_reconstructed);
            let superseded_review_id = ledger
                .active_review_cycle
                .as_ref()
                .and_then(|cycle| cycle.superseded_review_id.clone());
            let cycle_id = ledger
                .active_review_cycle
                .as_ref()
                .map(|cycle| cycle.cycle_id.clone())
                .unwrap_or_else(|| {
                    format!(
                        "cycle-{}-{}-after-agent",
                        ledger.completion_epoch, ledger.manifest_revision
                    )
                });
            ledger.active_review_cycle = Some(CompletionReviewCycle {
                cycle_id: cycle_id.clone(),
                manifest_revision: ledger.manifest_revision,
                parent_terminal_review_id,
                superseded_review_id,
                phase: CompletionReviewCyclePhase::InitialReviewPending,
                correction_consumed,
                manifest_gap_reconstructed,
                accepted_review_id: None,
                accepted_dossier_snapshot_id: None,
            });
            ledger.review_risk.unresolved = true;
            ledger.review_risk.cycle_id = Some(cycle_id);
            ledger.review_risk.opened_at = Some(timestamp());
            ledger.review_risk.resolved_at = None;
            document.completion = None;
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn invalidate_completion_after_terminal_emission_failure(
        &self,
        reason: &str,
    ) -> AtomicReviewTransition<()> {
        let Some(expected_revision) = self.document_revision().await else {
            return AtomicReviewTransition::Failed;
        };
        let reason = reason.to_string();
        self.atomic_review_update(expected_revision, None, None, move |document| {
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            let persisted_passed = document
                .completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
            if !persisted_passed {
                return;
            }
            let Some(superseded_review_id) = ledger.last_terminal_closure.clone() else {
                return;
            };
            let Some(superseded) = ledger
                .receipts
                .iter()
                .find(|receipt| receipt.review_id == superseded_review_id)
                .cloned()
            else {
                return;
            };
            let review_id = format!(
                "review-{}-{}-{}",
                ledger.completion_epoch, ledger.manifest_revision, ledger.next_review_sequence
            );
            ledger.next_review_sequence = ledger.next_review_sequence.saturating_add(1);
            ledger.receipts.push(CompletionReviewReceiptV2 {
                review_id: review_id.clone(),
                attempt_kind: CompletionReviewAttemptKind::TerminalClosure,
                parent_review_id: superseded.parent_review_id.clone(),
                superseded_review_id: Some(superseded_review_id),
                candidate_mutation_revision: superseded.candidate_mutation_revision,
                candidate_hash: superseded.candidate_hash.clone(),
                implementation_identity_hash: superseded.implementation_identity_hash.clone(),
                dossier_snapshot_id: superseded.dossier_snapshot_id.clone(),
                user_source_ledger_hash: superseded.user_source_ledger_hash.clone(),
                requirement_manifest_hash: superseded.requirement_manifest_hash,
                findings: Vec::new(),
                dispositions: Vec::new(),
                manifest_gaps: Vec::new(),
                repair_instruction_hash: None,
                repair_baseline: None,
                baseline_hash: None,
                input_mode: None,
                delta_hash: None,
                rereview_delta: None,
                fallback_reasons: Vec::new(),
                candidate_implementation_identity: None,
                rereview_audit_hash: None,
                infrastructure_outcome: format!("terminal_emission_failure:{reason}"),
                review_clean: false,
                terminal_outcome: Some("partial".to_string()),
                recorded_at: timestamp(),
            });
            if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                cycle.phase = CompletionReviewCyclePhase::TerminalPartial;
            }
            ledger.review_risk.unresolved = true;
            ledger.review_risk.resolved_at = None;
            ledger.last_terminal_closure = Some(review_id);
            let mut completion = document.completion.clone().unwrap_or(TaskCompletionGate {
                status: TaskCompletionStatus::Partial,
                reasons: Vec::new(),
                evidence_path: None,
            });
            completion.status = TaskCompletionStatus::Partial;
            completion.reasons.push(reason);
            completion.reasons.sort();
            completion.reasons.dedup();
            document.completion = Some(completion);
            document.updated_at = timestamp();
        })
        .await
    }

    pub(crate) async fn apply_source_classification(
        &self,
        dossier: &CompletionReviewDossier,
        materialization: SourceMaterialization,
    ) -> AtomicReviewTransition<()> {
        let Some(prepared) = prepare_source_materialization(dossier, materialization) else {
            return AtomicReviewTransition::Failed;
        };
        let PreparedSourceMaterialization {
            local_classifications,
            requirements,
            mappings,
        } = prepared;
        {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if document.revision != dossier.document_revision {
                return AtomicReviewTransition::Superseded;
            }
            let Some(ledger) = document.completion_review_v2.as_ref() else {
                return AtomicReviewTransition::Failed;
            };
            if !dossier_sources_are_current(ledger, &dossier.sources) {
                return AtomicReviewTransition::Failed;
            }
        }
        let replacement_cache_keys = local_classifications
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let replacement_cache_entries = local_classifications
            .into_iter()
            .map(|(key, classification)| SourceClassificationCacheEntry {
                contract_version: key.contract_version,
                source_kind: key.source_kind,
                content_hash: key.content_hash,
                classification,
            })
            .collect::<Vec<_>>();
        let classified_manifest_revision = dossier.manifest_revision.saturating_add(1);
        let manifest_hash = requirement_manifest_hash(classified_manifest_revision, &requirements);
        self.atomic_review_update(dossier.document_revision, None, None, move |document| {
            document
                .source_classification_cache
                .retain(|entry| !replacement_cache_keys.contains(&entry.key()));
            document
                .source_classification_cache
                .extend(replacement_cache_entries);
            let Some(ledger) = document.completion_review_v2.as_mut() else {
                return;
            };
            ledger
                .mapping_revisions
                .extend(
                    mappings
                        .into_iter()
                        .map(|(source_id, mapping)| SourceMappingRevision {
                            completion_epoch: ledger.completion_epoch,
                            manifest_revision: classified_manifest_revision,
                            source_id,
                            source_classification_contract_version: Some(
                                SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
                            ),
                            relationship_resolver_contract_version: Some(
                                RELATIONSHIP_RESOLVER_CONTRACT_VERSION.to_string(),
                            ),
                            mapping,
                        }),
                );
            ledger.manifest_snapshots.push(RequirementManifestSnapshot {
                completion_epoch: ledger.completion_epoch,
                manifest_revision: classified_manifest_revision,
                manifest_hash,
                requirements,
            });
            ledger.manifest_revision = classified_manifest_revision;
            if let Some(cycle) = ledger.active_review_cycle.as_mut() {
                cycle.cycle_id = format!(
                    "cycle-{}-{classified_manifest_revision}",
                    ledger.completion_epoch
                );
                cycle.manifest_revision = classified_manifest_revision;
                cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
            }
            document.evidence_epoch = document.evidence_epoch.saturating_add(1);
            document.desktop_activation_receipt = None;
            document.completion = None;
            document.updated_at = timestamp();
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn completion_review_evidence_summary(
        &self,
        gate: &TaskCompletionGate,
    ) -> String {
        let mut lines = vec![format!(
            "Evidence gate: {}",
            completion_status_name(gate.status)
        )];
        lines.extend(
            gate.reasons
                .iter()
                .map(|reason| format!("Gate reason: {reason}")),
        );

        let guard = self.document.lock().await;
        let Some(document) = guard.as_ref() else {
            return lines.join("\n");
        };
        for step in &document.plan {
            lines.push(format!(
                "Plan step {} [{:?}]: {}",
                step.id, step.status, step.step
            ));
            lines.extend(
                step.acceptance_criteria
                    .iter()
                    .map(|criterion| format!("Acceptance criterion: {criterion}")),
            );
        }
        for snapshot in document.latest_file_hashes.values() {
            lines.push(format!(
                "Changed path: {} (exists: {}, sha1: {})",
                snapshot.path,
                snapshot.exists,
                snapshot.sha1.as_deref().unwrap_or("unavailable")
            ));
        }
        let mut prior_epoch_receipt_count = 0usize;
        for receipt in &document.command_receipts {
            if receipt.epoch != document.evidence_epoch {
                prior_epoch_receipt_count = prior_epoch_receipt_count.saturating_add(1);
                continue;
            }
            let outcome = if receipt.timed_out {
                "timed_out"
            } else if receipt.exit_code == 0 {
                "succeeded"
            } else {
                "failed"
            };
            lines.push(format!(
                "Command receipt [current epoch {}, outcome: {}, possible mutation: {}]: {}",
                receipt.epoch,
                outcome,
                receipt.possible_mutation,
                receipt.command.join(" "),
            ));
        }
        if prior_epoch_receipt_count > 0 {
            lines.push(format!(
                "Prior-epoch command receipts omitted: {prior_epoch_receipt_count}"
            ));
        }
        lines.join("\n")
    }

    #[cfg(test)]
    pub(crate) async fn record_completion_review_audit(
        &self,
        turn_id: &str,
        outcome: &str,
        failure_category: Option<&str>,
        finding_summary: Vec<String>,
        repair_injected: bool,
    ) -> bool {
        if !self.allows_kd4_completion() {
            return false;
        }
        let Some(((), snapshot)) = self
            .update_document(|document| {
                document
                    .completion_review_receipts
                    .push(CompletionReviewAuditReceipt {
                        turn_id: turn_id.to_string(),
                        recorded_at: timestamp(),
                        evidence_epoch: document.evidence_epoch,
                        outcome: outcome.to_string(),
                        failure_category: failure_category.map(str::to_string),
                        finding_summary,
                        repair_injected,
                    });
                trim_to_last(
                    &mut document.completion_review_receipts,
                    MAX_COMPLETION_REVIEW_RECEIPTS,
                );
                document.updated_at = timestamp();
            })
            .await
        else {
            return false;
        };
        if self.persist_document(&snapshot).await != PersistOutcome::Persisted {
            warn!("failed to persist observational completion-review audit receipt");
            return false;
        }
        true
    }

    pub(crate) async fn completion_gate(&self) -> Option<TaskCompletionGate> {
        if !self.allows_kd4_completion() {
            return None;
        }
        let source_capture_failed = self.user_source_capture_failed();
        let mut latest_gate = None;
        for _ in 0..8 {
            self.refresh_external_file_freshness().await;
            let (gate, snapshot) = self
                .update_document(|document| {
                    if !task_is_tracked(document) {
                        return None;
                    }
                    if self.evidence_path.is_some() {
                        resolve_risk(document, "task-evidence-storage-failure");
                    }
                    let mut gate = derive_completion_gate(document, self.evidence_path.as_deref());
                    overlay_completion_review_gate(document, &mut gate, source_capture_failed);
                    document.completion = Some(gate.clone());
                    document.updated_at = timestamp();
                    Some(gate)
                })
                .await?;
            let gate = gate?;
            latest_gate = Some(gate.clone());
            match self.persist_document(&snapshot).await {
                PersistOutcome::Persisted => return Some(gate),
                PersistOutcome::Superseded => continue,
                PersistOutcome::Failed => {
                    return Some(
                        self.block_gate_for_persistence(
                            gate,
                            Some(snapshot.revision),
                            "task-evidence persistence failed; completion is not durably recorded",
                            true,
                        )
                        .await,
                    );
                }
            }
        }
        let gate = latest_gate?;
        Some(
            self.block_gate_for_persistence(
                gate,
                None,
                "task-evidence changed repeatedly while completion was being persisted; a stable completion snapshot was not recorded",
                false,
            )
            .await,
        )
    }

    async fn block_gate_for_persistence(
        &self,
        mut gate: TaskCompletionGate,
        snapshot_revision: Option<u64>,
        reason: &str,
        record_storage_risk: bool,
    ) -> TaskCompletionGate {
        gate.reasons.push(reason.to_string());
        gate.reasons.sort();
        gate.reasons.dedup();
        if gate.status != TaskCompletionStatus::Blocked {
            gate.status = TaskCompletionStatus::Partial;
        }
        let snapshot = {
            let mut guard = self.document.lock().await;
            guard.as_mut().map(|document| {
                if record_storage_risk {
                    upsert_risk(
                        document,
                        task_evidence_storage_risk(reason, document.evidence_epoch),
                    );
                }
                if snapshot_revision.is_some() && snapshot_revision != Some(document.revision) {
                    let current_gate =
                        derive_completion_gate(document, self.evidence_path.as_deref());
                    if current_gate.status == TaskCompletionStatus::Blocked {
                        gate.status = TaskCompletionStatus::Blocked;
                    }
                    gate.reasons.extend(current_gate.reasons);
                    gate.reasons.sort();
                    gate.reasons.dedup();
                }
                document.completion = Some(gate.clone());
                document.updated_at = timestamp();
                document.revision = document.revision.saturating_add(1);
                document.clone()
            })
        };
        if let Some(snapshot) = snapshot {
            // The original write may have failed transiently. Persist the blocking
            // storage risk once more without recursing if storage remains unavailable.
            let _ = self.persist_document(&snapshot).await;
        }
        gate
    }

    async fn refresh_external_file_freshness(&self) {
        if !self.allows_kd4_completion() {
            return;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return;
        };
        let (expected, previous_artifacts, artifact_paths) = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .map(|document| {
                    (
                        document.latest_file_hashes.clone(),
                        document.latest_generated_artifact_hashes.clone(),
                        document
                            .generated_artifact_requirements
                            .iter()
                            .filter_map(|requirement| requirement.path.clone())
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .unwrap_or_default()
        };
        if expected.is_empty() && artifact_paths.is_empty() {
            return;
        }
        let mut changed = Vec::new();
        for (path, previous) in expected {
            let current = snapshot_file(repo_root, &path).await;
            if current != previous {
                changed.push((previous, current));
            }
        }
        let mut current_artifacts = BTreeMap::new();
        let mut changed_artifacts = false;
        for path in artifact_paths {
            let current = snapshot_generated_artifact(repo_root, &path).await;
            changed_artifacts |= previous_artifacts.get(&path) != Some(&current);
            current_artifacts.insert(path, current);
        }
        changed_artifacts |= previous_artifacts.len() != current_artifacts.len();
        if changed.is_empty() && !changed_artifacts {
            return;
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                let changed = changed
                    .into_iter()
                    .filter(|(previous, current)| {
                        document.latest_file_hashes.get(&current.path) == Some(previous)
                    })
                    .map(|(_, current)| current)
                    .collect::<Vec<_>>();
                let artifact_state_is_current =
                    document.latest_generated_artifact_hashes == previous_artifacts;
                if changed.is_empty() && (!changed_artifacts || !artifact_state_is_current) {
                    return;
                }
                let prior_artifact_state_exists = changed_artifacts
                    && !previous_artifacts.is_empty()
                    && artifact_state_is_current;
                let changed_files = !changed.is_empty();
                if changed_files || prior_artifact_state_exists {
                    invalidate_for_mutation(document);
                }
                let epoch = document.evidence_epoch;
                for current in changed {
                    let path = current.path.clone();
                    if current.read_error.is_some() {
                        upsert_risk(document, unreadable_file_risk(&path, epoch, "freshness"));
                    } else {
                        resolve_risk(document, &unreadable_file_risk_id(&path));
                    }
                    document.latest_file_hashes.insert(path.clone(), current);
                    for step in &mut document.plan {
                        if step.edit_paths.contains(&path)
                            && !matches!(step.status, StepStatus::Blocked | StepStatus::Skipped)
                        {
                            step.status = StepStatus::Implemented;
                        }
                    }
                }
                document.latest_generated_artifact_hashes = current_artifacts;
                if changed_files || prior_artifact_state_exists {
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("external-change-{epoch}"),
                            description: "a task-controlled file changed after its recorded state"
                                .to_string(),
                            source: "freshness".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
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

    pub(crate) async fn reconcile_default_child_workspace_events(
        &self,
        events: &[codex_agent_task_store::WorkspaceEvent],
        root_session_id: &str,
        same_root_typed_actor_ids: &BTreeSet<String>,
    ) -> bool {
        if self.mode != TaskEvidenceMode::Kd4Completion {
            return true;
        }
        let Some(repo_root) = self.repo_root.as_ref() else {
            return false;
        };
        let cursor = {
            let guard = self.document.lock().await;
            guard
                .as_ref()
                .and_then(|document| document.completion_review_v2.as_ref())
                .map(|ledger| ledger.last_workspace_event_epoch)
                .unwrap_or_default()
        };
        let scanned = events
            .iter()
            .filter(|event| event.epoch > cursor)
            .cloned()
            .collect::<Vec<_>>();
        if scanned.is_empty() {
            return true;
        }
        let max_epoch = scanned
            .iter()
            .map(|event| event.epoch)
            .max()
            .unwrap_or(cursor);
        let legacy_actor_prefix = format!("legacy:{root_session_id}:");
        let root_actor_id = format!("root:{root_session_id}");
        let accepted = scanned
            .iter()
            .filter(|event| match event.actor_kind {
                codex_agent_task_store::WorkspaceActorKind::Root => {
                    event.actor_id.as_deref() == Some(root_actor_id.as_str())
                }
                codex_agent_task_store::WorkspaceActorKind::Legacy => event
                    .actor_id
                    .as_deref()
                    .is_some_and(|actor_id| actor_id.starts_with(&legacy_actor_prefix)),
                codex_agent_task_store::WorkspaceActorKind::Typed
                | codex_agent_task_store::WorkspaceActorKind::External => false,
            })
            .cloned()
            .collect::<Vec<_>>();
        let unattributed_epochs = scanned
            .iter()
            .filter(|event| match event.actor_kind {
                codex_agent_task_store::WorkspaceActorKind::External => true,
                codex_agent_task_store::WorkspaceActorKind::Root => {
                    event.actor_id.as_deref() != Some(root_actor_id.as_str())
                }
                codex_agent_task_store::WorkspaceActorKind::Legacy => event
                    .actor_id
                    .as_deref()
                    .is_none_or(|actor_id| !actor_id.starts_with(&legacy_actor_prefix)),
                codex_agent_task_store::WorkspaceActorKind::Typed => event
                    .actor_id
                    .as_ref()
                    .is_none_or(|actor_id| !same_root_typed_actor_ids.contains(actor_id)),
            })
            .map(|event| event.epoch)
            .collect::<Vec<_>>();
        let mut snapshots = BTreeMap::new();
        let mut repository_wide = false;
        for event in &accepted {
            for path in &event.paths {
                if path == codex_agent_task_store::REPOSITORY_WIDE_PATH {
                    repository_wide = true;
                    continue;
                }
                snapshots
                    .entry(path.clone())
                    .or_insert(snapshot_file(repo_root, path).await);
            }
        }

        let Some((_, snapshot)) = self
            .update_document(|document| {
                let Some(ledger) = document.completion_review_v2.as_mut() else {
                    return;
                };
                if ledger.last_workspace_event_epoch != cursor {
                    return;
                }
                ledger.last_workspace_event_epoch = max_epoch;
                if !accepted.is_empty() {
                    let has_unrepresented_mutation = repository_wide
                        || snapshots.iter().any(|(path, state)| {
                            document.latest_file_hashes.get(path) != Some(state)
                        });
                    if has_unrepresented_mutation {
                        invalidate_for_mutation(document);
                    }
                    let Some(ledger) = document.completion_review_v2.as_mut() else {
                        return;
                    };
                    for event in &accepted {
                        let Some(actor_id) = event.actor_id.clone() else {
                            continue;
                        };
                        if ledger.attributed_workspace_events.iter().any(|stored| {
                            stored.workspace_id == event.workspace_id
                                && stored.epoch == event.epoch
                                && stored.actor_id == actor_id
                        }) {
                            continue;
                        }
                        let mut paths = event.paths.clone();
                        paths.sort();
                        paths.dedup();
                        ledger
                            .attributed_workspace_events
                            .push(TaskAttributedWorkspaceEvent {
                                workspace_id: event.workspace_id.clone(),
                                epoch: event.epoch,
                                actor_id,
                                paths,
                            });
                    }
                    trim_to_last(
                        &mut ledger.attributed_workspace_events,
                        MAX_ATTRIBUTED_WORKSPACE_EVENTS,
                    );
                    for (path, state) in &snapshots {
                        document.latest_file_hashes.insert(path.clone(), state.clone());
                    }
                }
                if repository_wide || !unattributed_epochs.is_empty() {
                    let epoch = document.evidence_epoch;
                    upsert_risk(
                        document,
                        EvidenceRisk {
                            id: format!("unattributed-workspace-mutation-{epoch}"),
                            description: format!(
                                "concurrent or repository-wide mutation could not be attributed exactly (workspace epochs: {})",
                                unattributed_epochs
                                    .iter()
                                    .map(u64::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            source: "workspace-event".to_string(),
                            blocking: false,
                            resolved: false,
                            epoch,
                        },
                    );
                }
                document.updated_at = timestamp();
                document.completion = None;
            })
            .await
        else {
            return false;
        };
        self.persist_document(&snapshot).await == PersistOutcome::Persisted
    }

    async fn record_host_mutation(&self) {
        if self.mode == TaskEvidenceMode::Disabled {
            return;
        }
        let Some((_, snapshot)) = self
            .update_document(|document| {
                invalidate_for_mutation(document);
                document.updated_at = timestamp();
            })
            .await
        else {
            return;
        };
        if self.persist_document(&snapshot).await == PersistOutcome::Failed {
            warn!("failed to persist task-evidence host mutation revision");
        }
    }

    #[cfg(test)]
    pub(crate) async fn record_external_mcp_evidence(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
    ) -> ExternalEvidenceCapture {
        self.record_external_mcp_evidence_with_limit(
            server_name,
            tool_name,
            call_id,
            result,
            None,
            None,
            MAX_EXTERNAL_EVIDENCE_RECEIPTS,
        )
        .await
    }

    pub(crate) async fn record_external_mcp_evidence_bound_with_provenance(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
    ) -> ExternalEvidenceCapture {
        self.record_external_mcp_evidence_with_limit(
            server_name,
            tool_name,
            call_id,
            result,
            provenance,
            implementation_identity_hash,
            MAX_EXTERNAL_EVIDENCE_RECEIPTS,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn record_external_mcp_evidence_with_limit(
        &self,
        server_name: &str,
        tool_name: &str,
        call_id: &str,
        result: &CallToolResult,
        provenance: Option<&ChildEvidenceProvenance>,
        implementation_identity_hash: Option<&str>,
        max_receipts: usize,
    ) -> ExternalEvidenceCapture {
        if self.mode == TaskEvidenceMode::Disabled {
            return ExternalEvidenceCapture::Ignored;
        }
        let metadata = match extract_external_evidence_metadata(result) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => return ExternalEvidenceCapture::Ignored,
            Err(message) => return ExternalEvidenceCapture::Warning(message),
        };
        let Some(evidence_path) = self.evidence_path.clone() else {
            return ExternalEvidenceCapture::Warning(
                "external evidence persistence is unavailable for this task",
            );
        };
        let external_evidence_permit = match Arc::clone(&self.external_evidence_gate)
            .acquire_owned()
            .await
        {
            Ok(permit) => permit,
            Err(err) => {
                warn!("external evidence serialization gate unexpectedly closed: {err}");
                return ExternalEvidenceCapture::Warning(
                    "external evidence persistence is unavailable for this task",
                );
            }
        };
        let canonical_payload = canonical_mcp_result_payload(result);
        let canonical_bytes = match serde_json::to_vec(&canonical_payload) {
            Ok(bytes) => bytes,
            Err(_) => {
                return ExternalEvidenceCapture::Warning(
                    "external evidence result could not be canonicalized",
                );
            }
        };
        let result_sha256 = format!("{:x}", Sha256::digest(&canonical_bytes));
        let (task_epoch, step_id, workspace_root_fingerprint, host_mutation_revision) = {
            let guard = self.document.lock().await;
            let Some(document) = guard.as_ref() else {
                return ExternalEvidenceCapture::Ignored;
            };
            (
                document.evidence_epoch,
                (self.mode == TaskEvidenceMode::Kd4Completion)
                    .then(|| document.active_step_id.clone())
                    .flatten(),
                workspace_root_fingerprint(&document.start),
                Some(document.host_mutation_revision),
            )
        };

        let (payload, payload_artifact_id, mut pending_artifact) = if canonical_bytes.len()
            <= EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES
        {
            (Some(canonical_payload), None, None)
        } else {
            let Some(codex_home) = self.codex_home.as_deref() else {
                return ExternalEvidenceCapture::Warning(
                    "external evidence payload storage is unavailable",
                );
            };
            let Some(thread_id) = self.thread_id.as_deref() else {
                return ExternalEvidenceCapture::Warning(
                    "external evidence thread identity is unavailable",
                );
            };
            let Some(artifact_bytes) = encode_external_evidence_artifact(&canonical_bytes) else {
                return ExternalEvidenceCapture::Warning(
                    "external evidence payload artifact could not be encoded",
                );
            };
            let pending = crate::tools::command_output_artifact::create_evidence_output_artifact(
                codex_home,
                thread_id,
                &artifact_bytes,
            )
            .await;
            let pending = match pending {
                Ok(pending) => pending,
                Err(err) => {
                    warn!("external evidence payload artifact could not be stored: {err}");
                    return ExternalEvidenceCapture::Warning(
                        "external evidence payload artifact could not be stored",
                    );
                }
            };
            let artifact_id = pending.id().to_string();
            let summary = serde_json::json!({
                "evidenceMetaSummary": {
                    "producer": metadata.producer.clone(),
                    "schemaVersion": metadata.producer_schema_version,
                    "payloadCompleteness": evidence_completeness_name(
                        metadata.payload_completeness
                    ),
                },
                "structuredFieldCount": result
                    .structured_content
                    .as_ref()
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len),
                "contentItems": result.content.len(),
                "isError": result.is_error,
                "artifact": {
                    "id": artifact_id,
                    "encoding": "KD4_EXTERNAL_EVIDENCE_CANONICAL_JSON_STRING_CHUNKS_V1"
                }
            });
            debug_assert!(
                serde_json::to_vec(&summary)
                    .is_ok_and(|bytes| { bytes.len() <= EXTERNAL_EVIDENCE_INLINE_PAYLOAD_BYTES }),
                "external evidence summary must remain within the inline payload cap"
            );
            (Some(summary), Some(artifact_id), Some(pending))
        };

        let document = Arc::clone(&self.document);
        let persistence_gate = Arc::clone(&self.persistence_gate);
        let last_persisted_revision = Arc::clone(&self.last_persisted_revision);
        let persistence_test_control = self.persistence_test_control();
        let codex_home = self.codex_home.clone();
        let thread_id = self.thread_id.clone();
        let mode = self.mode;
        let server_name = server_name.to_string();
        let tool_name = tool_name.to_string();
        let call_id = call_id.to_string();
        let tool_success = result.is_error != Some(true);
        let provenance = provenance.cloned();
        let implementation_identity_hash = implementation_identity_hash.map(str::to_string);
        let coordinator = tokio::spawn(async move {
            let _external_evidence_permit = external_evidence_permit;
            let mut persistence_permit = match Arc::clone(&persistence_gate).acquire_owned().await {
                Ok(permit) => permit,
                Err(err) => {
                    warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                    return ExternalEvidenceCapture::Warning(
                        "external evidence receipt could not be durably persisted",
                    );
                }
            };
            let mut receipt_id = String::new();
            let mut trimmed_receipts = Vec::new();
            let snapshot = {
                let mut guard = document.lock().await;
                let Some(document) = guard.as_mut() else {
                    return ExternalEvidenceCapture::Ignored;
                };
                let id = next_receipt_id(
                    "external-evidence",
                    &mut document.next_external_evidence_receipt_sequence,
                );
                receipt_id.clone_from(&id);
                document.external_evidence.push(ExternalEvidenceReceipt {
                    id,
                    producer: metadata.producer,
                    producer_schema_version: metadata.producer_schema_version,
                    server_name,
                    tool_name,
                    call_id,
                    source_thread_id: provenance
                        .as_ref()
                        .map(|provenance| provenance.source_thread_id.clone()),
                    source_agent_path: provenance
                        .as_ref()
                        .map(|provenance| provenance.source_agent_path.clone()),
                    recorded_at: timestamp(),
                    task_epoch,
                    step_id,
                    workspace_root_fingerprint,
                    host_mutation_revision,
                    implementation_identity_hash,
                    provider_snapshot: metadata.provider_snapshot,
                    tool_success,
                    payload_completeness: metadata.payload_completeness,
                    truncated: metadata.truncated,
                    approximate: metadata.approximate,
                    limitations: metadata.limitations,
                    result_sha256,
                    payload,
                    payload_artifact_id,
                });
                let trim_count = document
                    .external_evidence
                    .len()
                    .saturating_sub(max_receipts);
                trimmed_receipts.extend(document.external_evidence.drain(..trim_count));
                document.updated_at = timestamp();
                document.revision = document.revision.saturating_add(1);
                document.clone()
            };
            let (outcome, returned_permit) = persist_document_with_permit(
                evidence_path.clone(),
                snapshot,
                persistence_permit,
                Arc::clone(&last_persisted_revision),
                persistence_test_control.clone(),
            )
            .await;
            persistence_permit = match returned_permit {
                Some(permit) => permit,
                None => match Arc::clone(&persistence_gate).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        return ExternalEvidenceCapture::Warning(
                            "external evidence receipt could not be durably persisted",
                        );
                    }
                },
            };
            match outcome {
                PersistOutcome::Persisted | PersistOutcome::Superseded => {
                    if let Some(pending) = pending_artifact.take() {
                        let _ = pending.mark_durable();
                    }
                    drop(persistence_permit);
                    for receipt in trimmed_receipts {
                        delete_external_artifact_owned(
                            codex_home.as_deref(),
                            thread_id.as_deref(),
                            receipt.payload_artifact_id.as_deref(),
                        )
                        .await;
                    }
                    ExternalEvidenceCapture::Stored
                }
                PersistOutcome::Failed => {
                    let restored_receipts = trimmed_receipts;
                    let failure_snapshot = {
                        let mut guard = document.lock().await;
                        let Some(document) = guard.as_mut() else {
                            drop(pending_artifact.take());
                            return ExternalEvidenceCapture::Warning(
                                "external evidence receipt could not be durably persisted",
                            );
                        };
                        let mut retained = std::mem::take(&mut document.external_evidence);
                        retained.retain(|receipt| receipt.id != receipt_id);
                        let mut restored = restored_receipts;
                        restored.append(&mut retained);
                        document.external_evidence = restored;
                        if mode == TaskEvidenceMode::Kd4Completion {
                            upsert_risk(
                                document,
                                task_evidence_storage_risk(
                                    "external evidence receipt could not be durably persisted",
                                    document.evidence_epoch,
                                ),
                            );
                        }
                        document.updated_at = timestamp();
                        document.revision = document.revision.saturating_add(1);
                        document.clone()
                    };
                    drop(pending_artifact.take());
                    let rollback_revision = failure_snapshot.revision;
                    let (rollback_outcome, _) = persist_document_with_permit(
                        evidence_path,
                        failure_snapshot,
                        persistence_permit,
                        Arc::clone(&last_persisted_revision),
                        persistence_test_control,
                    )
                    .await;
                    if rollback_outcome == PersistOutcome::Failed {
                        last_persisted_revision.fetch_max(rollback_revision, Ordering::AcqRel);
                    }
                    ExternalEvidenceCapture::Warning(
                        "external evidence receipt could not be durably persisted",
                    )
                }
            }
        });
        match coordinator.await {
            Ok(capture) => capture,
            Err(err) => {
                warn!("external evidence persistence coordinator failed: {err}");
                ExternalEvidenceCapture::Warning(
                    "external evidence receipt could not be durably persisted",
                )
            }
        }
    }

    async fn document_revision(&self) -> Option<u64> {
        self.document
            .lock()
            .await
            .as_ref()
            .map(|document| document.revision)
    }

    async fn atomic_review_update<T: Send>(
        &self,
        expected_revision: u64,
        expected_implementation_identity: Option<&str>,
        expected_dossier_snapshot: Option<&str>,
        update: impl FnOnce(&mut TaskEvidenceDocument) -> T + Send,
    ) -> AtomicReviewTransition<T> {
        let Some(path) = self.evidence_path.clone() else {
            return AtomicReviewTransition::Failed;
        };
        let _permit = match Arc::clone(&self.persistence_gate).acquire_owned().await {
            Ok(permit) => permit,
            Err(err) => {
                warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                return AtomicReviewTransition::Failed;
            }
        };
        let mut guard = Arc::clone(&self.document).lock_owned().await;
        let Some(document) = guard.as_ref() else {
            return AtomicReviewTransition::Failed;
        };
        if document.revision != expected_revision
            || !review_identity_is_current(
                document,
                expected_implementation_identity,
                expected_dossier_snapshot,
            )
        {
            return AtomicReviewTransition::Superseded;
        }

        let mut candidate = document.clone();
        let result = update(&mut candidate);
        candidate.schema_version = TASK_EVIDENCE_SCHEMA_VERSION;
        candidate.source_classification_cache = canonical_source_classification_cache(
            std::mem::take(&mut candidate.source_classification_cache),
        );
        candidate.revision = candidate.revision.saturating_add(1);
        let bytes = match serde_json::to_vec_pretty(&candidate) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("failed to serialize KD4 task evidence transition: {err}");
                return AtomicReviewTransition::Failed;
            }
        };
        let test_control = self.persistence_test_control();
        let write_result = tokio::task::spawn_blocking(move || {
            if let Some(control) = test_control.as_ref()
                && let Some((started, release)) = control
                    .before_next_write
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                started.wait();
                release.wait();
            }
            if test_control
                .as_ref()
                .is_some_and(|control| control.fail_writes.load(Ordering::Acquire))
            {
                Err(io::Error::other(
                    "injected task-evidence persistence failure",
                ))
            } else {
                atomic_write_evidence(&path, &bytes)
            }
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                self.last_persisted_revision
                    .store(candidate.revision, Ordering::Release);
                *guard = Some(candidate);
                AtomicReviewTransition::Persisted(result)
            }
            Ok(Err(err)) => {
                warn!("failed to persist atomic KD4 completion-review transition: {err}");
                AtomicReviewTransition::Failed
            }
            Err(err) => {
                warn!("KD4 completion-review persistence task failed: {err}");
                AtomicReviewTransition::Failed
            }
        }
    }

    async fn update_document<T>(
        &self,
        update: impl FnOnce(&mut TaskEvidenceDocument) -> T,
    ) -> Option<(T, TaskEvidenceDocument)> {
        let mut guard = self.document.lock().await;
        let document = guard.as_mut()?;
        let result = update(document);
        document.revision = document.revision.saturating_add(1);
        Some((result, document.clone()))
    }

    async fn persist_document(&self, document: &TaskEvidenceDocument) -> PersistOutcome {
        let Some(path) = self.evidence_path.as_ref() else {
            return PersistOutcome::Persisted;
        };
        let permit = match Arc::clone(&self.persistence_gate).acquire_owned().await {
            Ok(permit) => permit,
            Err(err) => {
                warn!("KD4 task-evidence persistence gate unexpectedly closed: {err}");
                return PersistOutcome::Failed;
            }
        };
        persist_document_with_permit(
            path.clone(),
            document.clone(),
            permit,
            Arc::clone(&self.last_persisted_revision),
            self.persistence_test_control(),
        )
        .await
        .0
    }

    fn persistence_test_control(&self) -> Option<PersistenceTestControl> {
        #[cfg(test)]
        {
            return self
                .persistence_test_control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
        }
        #[cfg(not(test))]
        None
    }
}

fn review_identity_is_current(
    document: &TaskEvidenceDocument,
    expected_implementation_identity: Option<&str>,
    expected_dossier_snapshot: Option<&str>,
) -> bool {
    if expected_implementation_identity.is_none() && expected_dossier_snapshot.is_none() {
        return true;
    }
    let Some(ledger) = document.completion_review_v2.as_ref() else {
        return false;
    };
    let accepted_review_id = ledger
        .active_review_cycle
        .as_ref()
        .and_then(|cycle| cycle.accepted_review_id.as_deref());
    let Some(receipt) = accepted_review_id.and_then(|review_id| {
        ledger
            .receipts
            .iter()
            .find(|receipt| receipt.review_id == review_id)
    }) else {
        return false;
    };
    expected_implementation_identity
        .is_none_or(|expected| receipt.implementation_identity_hash == expected)
        && expected_dossier_snapshot.is_none_or(|expected| receipt.dossier_snapshot_id == expected)
}

async fn user_source_material(
    input: &UserInput,
) -> Option<(UserSourceKind, String, UserSourceAvailability)> {
    #[allow(unreachable_patterns)]
    match input {
        UserInput::Text { text, .. } if !text.is_empty() => Some((
            UserSourceKind::Text,
            text.clone(),
            UserSourceAvailability::Available,
        )),
        UserInput::Image { image_url, .. } => Some((
            UserSourceKind::Image,
            image_url.clone(),
            if image_url.is_empty() {
                UserSourceAvailability::Unavailable
            } else {
                UserSourceAvailability::Available
            },
        )),
        UserInput::LocalImage { path, .. } => Some(
            file_backed_source_material(UserSourceKind::Image, "local-image", None, path).await,
        ),
        UserInput::Skill { name, path } => Some(
            file_backed_source_material(UserSourceKind::Attachment, "skill", Some(name), path)
                .await,
        ),
        UserInput::Mention { name, path } => Some((
            UserSourceKind::Attachment,
            format!("mention:{name}:{path}"),
            UserSourceAvailability::Available,
        )),
        _ => None,
    }
}

async fn file_backed_source_material(
    kind: UserSourceKind,
    reference_kind: &str,
    name: Option<&str>,
    path: &Path,
) -> (UserSourceKind, String, UserSourceAvailability) {
    let normalized_path = normalize_path_for_identity(path);
    let reference = match name {
        Some(name) => format!("{reference_kind}:{name}:{normalized_path}"),
        None => format!("{reference_kind}:{normalized_path}"),
    };
    match sha256_file(path).await {
        Ok(sha256) => (
            kind,
            format!("{reference}#sha256={sha256}"),
            UserSourceAvailability::Available,
        ),
        Err(_) => (
            kind,
            format!("{reference}#unavailable"),
            UserSourceAvailability::Unavailable,
        ),
    }
}

pub(crate) fn material_for_span(source: &UserSourceRecord, span: &SourceSpan) -> Option<String> {
    match (source.source_kind, span) {
        (UserSourceKind::Text, SourceSpan::Text { start, end })
            if start < end
                && *end <= source.exact_material.len()
                && source.exact_material.is_char_boundary(*start)
                && source.exact_material.is_char_boundary(*end) =>
        {
            source.exact_material.get(*start..*end).map(str::to_string)
        }
        (UserSourceKind::Image, SourceSpan::Image { reference, region })
            if reference == &source.exact_material =>
        {
            Some(match region {
                Some(region) if !region.trim().is_empty() => {
                    format!("{reference}#region={region}")
                }
                _ => reference.clone(),
            })
        }
        (UserSourceKind::Attachment, SourceSpan::Attachment { reference, range })
            if reference == &source.exact_material =>
        {
            Some(match range {
                Some(range) if !range.trim().is_empty() => format!("{reference}#range={range}"),
                _ => reference.clone(),
            })
        }
        _ => None,
    }
}

pub(crate) fn source_local_classifications_with_manifest_gaps(
    dossier: &CompletionReviewDossier,
    gaps: &[ManifestGapInput],
) -> Option<BTreeMap<SourceClassificationCacheKey, SourceLocalClassification>> {
    if gaps.is_empty() {
        return None;
    }
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let existing = dossier
        .requirements
        .iter()
        .map(|requirement| ClassifiedRequirementRef {
            source_id: requirement.source_id.clone(),
            source_span: requirement.source_span.clone(),
        })
        .collect::<BTreeSet<_>>();
    let expected_keys = dossier
        .sources
        .iter()
        .map(source_classification_cache_key)
        .collect::<BTreeSet<_>>();
    let mut corrected = expected_keys
        .iter()
        .map(|key| {
            dossier
                .source_classification_cache
                .get(key)
                .cloned()
                .map(|classification| (key.clone(), classification))
        })
        .collect::<Option<BTreeMap<_, _>>>()?;
    let mut seen = BTreeSet::new();
    const CORRECTION_REASON: &str =
        "Validated immutable manifest gap establishes an omitted requirement span.";
    for gap in gaps {
        let source = sources.get(gap.source_id.as_str())?;
        if gap.omitted_spans.is_empty() {
            return None;
        }
        for span in &gap.omitted_spans {
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: span.clone(),
            };
            material_for_span(source, span)?;
            if existing.contains(&reference) || !seen.insert(reference) {
                return None;
            }
            let key = source_classification_cache_key(source);
            let classification = corrected.get_mut(&key)?;
            classification.local_kind = SourceLocalClassificationKind::RequirementBearing;
            classification.requirement_spans.push(span.clone());
            classification.requirement_spans.sort();
            classification.requirement_spans.dedup();
            classification.local_semantic_cues.push(LocalSemanticCue {
                kind: LocalSemanticCueKind::Assertion,
                source_span: Some(span.clone()),
            });
            classification.local_semantic_cues.sort();
            classification.local_semantic_cues.dedup();
            if !classification.reason.contains(CORRECTION_REASON) {
                classification.reason =
                    format!("{} {CORRECTION_REASON}", classification.reason.trim())
                        .trim()
                        .to_string();
            }
        }
    }
    if corrected.len() != expected_keys.len()
        || dossier.sources.iter().any(|source| {
            corrected
                .get(&source_classification_cache_key(source))
                .is_none_or(|classification| {
                    !source_local_classification_is_valid_for_source(source, classification)
                })
        })
    {
        return None;
    }
    Some(corrected)
}

fn prepare_source_materialization(
    dossier: &CompletionReviewDossier,
    materialization: SourceMaterialization,
) -> Option<PreparedSourceMaterialization> {
    let SourceMaterialization {
        local_classifications,
        resolved_sources,
    } = materialization;
    let expected_keys = dossier
        .sources
        .iter()
        .map(source_classification_cache_key)
        .collect::<BTreeSet<_>>();
    if local_classifications.len() != expected_keys.len()
        || local_classifications
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_keys
        || resolved_sources.len() != dossier.sources.len()
        || resolved_sources
            .iter()
            .zip(&dossier.sources)
            .any(|(resolved, source)| resolved.source_id != source.source_id)
    {
        return None;
    }

    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut requirement_ids = BTreeMap::<ClassifiedRequirementRef, String>::new();
    for (resolved, source) in resolved_sources.iter().zip(&dossier.sources) {
        let local = local_classifications.get(&source_classification_cache_key(source))?;
        if !source_local_classification_is_valid_for_source(source, local) {
            return None;
        }
        let expected_kind = match local.local_kind {
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
        let mut resolved_spans = resolved
            .requirements
            .iter()
            .map(|requirement| requirement.source_span.clone())
            .collect::<Vec<_>>();
        resolved_spans.sort();
        if resolved.kind != expected_kind
            || resolved_spans != local.requirement_spans
            || matches!(
                local.local_kind,
                SourceLocalClassificationKind::NonRequirement
                    | SourceLocalClassificationKind::RelationshipOnlyContext
            ) && resolved.reason.as_deref() != Some(local.reason.as_str())
        {
            return None;
        }
        for requirement in &resolved.requirements {
            material_for_span(source, &requirement.source_span)?;
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: requirement.source_span.clone(),
            };
            let requirement_id = deterministic_requirement_id(source, &requirement.source_span);
            if requirement_ids.insert(reference, requirement_id).is_some() {
                return None;
            }
        }
    }
    if resolved_sources.iter().any(|resolved| {
        resolved
            .requirements
            .iter()
            .any(|requirement| match requirement.status {
                RequirementStatus::Active | RequirementStatus::Withdrawn => {
                    requirement.superseded_by.is_some()
                }
                RequirementStatus::Superseded => requirement
                    .superseded_by
                    .as_ref()
                    .is_none_or(|target| !requirement_ids.contains_key(target)),
            })
    }) {
        return None;
    }

    let mut requirements = Vec::new();
    let mut mappings = Vec::new();
    for resolved in resolved_sources {
        let source = sources.get(resolved.source_id.as_str())?;
        let mut mapped_requirement_ids = Vec::new();
        for requirement in resolved.requirements {
            let reference = ClassifiedRequirementRef {
                source_id: source.source_id.clone(),
                source_span: requirement.source_span.clone(),
            };
            let requirement_id = requirement_ids.get(&reference)?.clone();
            let exact_material = material_for_span(source, &requirement.source_span)?;
            mapped_requirement_ids.push(requirement_id.clone());
            requirements.push(RequirementRecord {
                requirement_id,
                source_id: source.source_id.clone(),
                source_content_hash: source.content_hash.clone(),
                exact_material,
                source_span: requirement.source_span,
                status: requirement.status,
                superseded_by: requirement
                    .superseded_by
                    .as_ref()
                    .and_then(|target| requirement_ids.get(target))
                    .cloned(),
            });
        }
        mapped_requirement_ids.sort();
        let mapping = match resolved.kind {
            ClassifiedSourceKind::RequirementBearing => SourceMapping::RequirementBearing {
                requirement_ids: mapped_requirement_ids,
            },
            ClassifiedSourceKind::NonRequirement => SourceMapping::NonRequirement {
                reason: resolved.reason.unwrap_or_default(),
            },
            ClassifiedSourceKind::SupersededContext => SourceMapping::SupersededContext {
                reason: resolved.reason.unwrap_or_default(),
            },
            ClassifiedSourceKind::UnavailableOrTruncated => SourceMapping::UnavailableOrTruncated,
        };
        mappings.push((source.source_id.clone(), mapping));
    }
    requirements.sort_by(|left, right| left.requirement_id.cmp(&right.requirement_id));
    if !requirement_supersession_is_acyclic(&requirements) {
        return None;
    }
    let next_requirements = requirements
        .iter()
        .map(|requirement| (requirement.requirement_id.as_str(), requirement))
        .collect::<BTreeMap<_, _>>();
    for previous in &dossier.requirements {
        let next = next_requirements.get(previous.requirement_id.as_str())?;
        if next.source_id != previous.source_id
            || next.source_content_hash != previous.source_content_hash
            || next.source_span != previous.source_span
            || next.exact_material != previous.exact_material
            || dossier.relationship_resolution_current
                && match previous.status {
                    RequirementStatus::Active => false,
                    RequirementStatus::Superseded => {
                        next.status != RequirementStatus::Superseded
                            || next.superseded_by != previous.superseded_by
                    }
                    RequirementStatus::Withdrawn => {
                        next.status != RequirementStatus::Withdrawn || next.superseded_by.is_some()
                    }
                }
        {
            return None;
        }
    }
    Some(PreparedSourceMaterialization {
        local_classifications,
        requirements,
        mappings,
    })
}

fn prepared_materialization_covers_manifest_gaps(
    dossier: &CompletionReviewDossier,
    gaps: &[ManifestGapInput],
    prepared: &PreparedSourceMaterialization,
) -> bool {
    if gaps.is_empty() {
        return false;
    }
    let sources = dossier
        .sources
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let existing = dossier
        .requirements
        .iter()
        .map(|requirement| ClassifiedRequirementRef {
            source_id: requirement.source_id.clone(),
            source_span: requirement.source_span.clone(),
        })
        .collect::<BTreeSet<_>>();
    let prepared_requirements = prepared
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
    let mut seen = BTreeSet::new();

    gaps.iter().all(|gap| {
        let Some(source) = sources.get(gap.source_id.as_str()) else {
            return false;
        };
        let key = source_classification_cache_key(source);
        let Some(local) = prepared.local_classifications.get(&key) else {
            return false;
        };
        !gap.omitted_spans.is_empty()
            && gap.omitted_spans.iter().all(|span| {
                let reference = ClassifiedRequirementRef {
                    source_id: source.source_id.clone(),
                    source_span: span.clone(),
                };
                let Some(exact_material) = material_for_span(source, span) else {
                    return false;
                };
                let Some(requirement) = prepared_requirements.get(&reference) else {
                    return false;
                };
                !existing.contains(&reference)
                    && seen.insert(reference)
                    && local.local_kind == SourceLocalClassificationKind::RequirementBearing
                    && local.requirement_spans.binary_search(span).is_ok()
                    && requirement.requirement_id == deterministic_requirement_id(source, span)
                    && requirement.source_content_hash == source.content_hash
                    && requirement.exact_material == exact_material
            })
    })
}

fn dossier_sources_are_current(
    ledger: &CompletionReviewLedgerV2,
    expected_sources: &[UserSourceRecord],
) -> bool {
    let mut current_sources = ledger
        .source_records
        .values()
        .filter(|source| source.completion_epoch == ledger.completion_epoch)
        .cloned()
        .collect::<Vec<_>>();
    current_sources.sort_by_key(|source| (source.source_ordinal, source.content_ordinal));
    current_sources == expected_sources
        && current_sources.iter().all(|source| {
            source.content_hash
                == user_source_content_hash(
                    source.source_kind,
                    &source.exact_material,
                    source.availability,
                )
        })
}

pub(crate) fn deterministic_requirement_id(source: &UserSourceRecord, span: &SourceSpan) -> String {
    format!(
        "REQ-{}",
        canonical_hash(
            REQUIREMENT_MANIFEST_CANONICAL_FORMAT,
            &serde_json::json!({
                "sourceId": source.source_id,
                "sourceContentHash": source.content_hash,
                "sourceSpan": span,
            }),
        )
    )
}

fn user_source_content_hash(
    kind: UserSourceKind,
    exact_material: &str,
    availability: UserSourceAvailability,
) -> String {
    canonical_hash(
        USER_SOURCE_LEDGER_CANONICAL_FORMAT,
        &serde_json::json!({
            "kind": kind,
            "material": exact_material,
            "availability": availability,
        }),
    )
}

fn deterministic_source_id(
    root_task_id: &str,
    completion_epoch: u64,
    message_id: &str,
    content_ordinal: u64,
    content_hash: &str,
) -> String {
    format!(
        "SRC-{}",
        canonical_hash(
            USER_SOURCE_LEDGER_CANONICAL_FORMAT,
            &serde_json::json!({
                "rootTaskId": root_task_id,
                "completionEpoch": completion_epoch,
                "messageId": message_id,
                "contentOrdinal": content_ordinal,
                "contentHash": content_hash,
            }),
        )
    )
}

fn source_mapping_revisions_for(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
) -> BTreeMap<String, SourceMappingRevision> {
    ledger
        .mapping_revisions
        .iter()
        .filter(|mapping| {
            mapping.completion_epoch == completion_epoch
                && mapping.manifest_revision == manifest_revision
        })
        .map(|mapping| (mapping.source_id.clone(), mapping.clone()))
        .collect()
}

fn source_mappings_for(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
) -> BTreeMap<String, SourceMapping> {
    source_mapping_revisions_for(ledger, completion_epoch, manifest_revision)
        .into_iter()
        .map(|(source_id, revision)| (source_id, revision.mapping))
        .collect()
}

fn user_source_ledger_snapshot_hash(
    ledger: &CompletionReviewLedgerV2,
    completion_epoch: u64,
    manifest_revision: u64,
    source_capture_failed: bool,
) -> String {
    let mut sources = ledger
        .source_records
        .values()
        .filter(|source| {
            source.completion_epoch == completion_epoch
                && source.introduced_manifest_revision <= manifest_revision
        })
        .cloned()
        .collect::<Vec<_>>();
    sources.sort_by_key(|source| (source.source_ordinal, source.content_ordinal));
    let mappings = source_mappings_for(ledger, completion_epoch, manifest_revision);
    canonical_hash(
        USER_SOURCE_LEDGER_CANONICAL_FORMAT,
        &serde_json::json!({
            "rootTaskId": ledger.root_task_id,
            "completionEpoch": completion_epoch,
            "manifestRevision": manifest_revision,
            "sources": sources,
            "mappings": mappings,
            "sourceCaptureFailed": source_capture_failed,
        }),
    )
}

fn active_source_mappings(ledger: &CompletionReviewLedgerV2) -> BTreeMap<String, SourceMapping> {
    source_mappings_for(ledger, ledger.completion_epoch, ledger.manifest_revision)
}

fn active_manifest(ledger: &CompletionReviewLedgerV2) -> Option<&RequirementManifestSnapshot> {
    ledger.manifest_snapshots.iter().rev().find(|manifest| {
        manifest.completion_epoch == ledger.completion_epoch
            && manifest.manifest_revision == ledger.manifest_revision
    })
}

fn requirement_manifest_hash(manifest_revision: u64, requirements: &[RequirementRecord]) -> String {
    let mut requirements = requirements.to_vec();
    requirements.sort_by(|left, right| left.requirement_id.cmp(&right.requirement_id));
    canonical_hash(
        REQUIREMENT_MANIFEST_CANONICAL_FORMAT,
        &serde_json::json!({
            "manifestRevision": manifest_revision,
            "requirements": requirements,
        }),
    )
}

fn completion_contract_hashes(
    document: &TaskEvidenceDocument,
    source_capture_failed: bool,
) -> Option<(u64, String, String)> {
    let ledger = document.completion_review_v2.as_ref()?;
    let requirements = active_manifest(ledger)
        .map(|manifest| manifest.requirements.clone())
        .unwrap_or_default();
    let source_hash = user_source_ledger_snapshot_hash(
        ledger,
        ledger.completion_epoch,
        ledger.manifest_revision,
        source_capture_failed,
    );
    let manifest_hash = requirement_manifest_hash(ledger.manifest_revision, &requirements);
    Some((ledger.manifest_revision, source_hash, manifest_hash))
}

fn canonical_hash(format_name: &str, value: &Value) -> String {
    let canonical = canonicalize_json_value(value.clone());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(format_name.as_bytes());
    bytes.push(b'\n');
    let Ok(serialized) = serde_json::to_vec(&canonical) else {
        unreachable!("canonical task evidence must serialize");
    };
    bytes.extend_from_slice(&serialized);
    format!("{:x}", Sha256::digest(bytes))
}

fn desktop_activation_receipt_is_complete(_receipt: &DesktopActivationReceipt) -> bool {
    // Desktop activation proof remains planned until a host-owned, publish-ID-bound
    // initialization handshake exists. Legacy receipts came from command stdout and
    // must never satisfy the completion gate, even when loaded from persisted state.
    false
}

pub(crate) async fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; FILE_HASH_CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalize_path_for_identity(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_repair_path(
    path: &str,
    allow_wildcards: bool,
) -> Result<String, RereviewFallbackReason> {
    let normalized = path.replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.as_bytes().get(1) == Some(&b':')
    {
        return Err(RereviewFallbackReason::InvalidPath);
    }
    let mut segments = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(RereviewFallbackReason::InvalidPath),
            _ if !allow_wildcards && segment.contains('*') => {
                return Err(RereviewFallbackReason::InvalidPath);
            }
            _ => segments.push(segment),
        }
    }
    if segments.is_empty() {
        return Err(RereviewFallbackReason::InvalidPath);
    }
    let canonical = segments.join("/");
    if cfg!(windows) {
        if !canonical.is_ascii() {
            return Err(RereviewFallbackReason::AmbiguousWindowsCase);
        }
        Ok(canonical.to_ascii_lowercase())
    } else {
        Ok(canonical)
    }
}

fn generated_pattern_is_supported(pattern: &str, grammar_version: u32) -> bool {
    if grammar_version != REPAIR_PATH_GRAMMAR_VERSION {
        return false;
    }
    let Ok(pattern) = canonical_repair_path(pattern, true) else {
        return false;
    };
    let mut recursive_wildcards = 0;
    for segment in pattern.split('/') {
        match segment {
            "*" => {}
            "**" => recursive_wildcards += 1,
            literal if !literal.contains('*') && !literal.contains('?') => {}
            _ => return false,
        }
    }
    recursive_wildcards <= 1
}

fn generated_pattern_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", tail)) => (0..=path.len().min(REPAIR_RECURSIVE_WILDCARD_MAX_SEGMENTS))
            .any(|consumed| generated_pattern_matches(tail, &path[consumed..])),
        Some((&"*", tail)) => path
            .split_first()
            .is_some_and(|(_, rest)| generated_pattern_matches(tail, rest)),
        Some((literal, tail)) => path
            .split_first()
            .is_some_and(|(value, rest)| value == literal && generated_pattern_matches(tail, rest)),
    }
}

fn repair_scope_matches(
    scope: &RepairPathScope,
    path: &str,
) -> Result<bool, RereviewFallbackReason> {
    let path = canonical_repair_path(path, false)?;
    match scope {
        RepairPathScope::ExactFile { path: expected } => {
            Ok(canonical_repair_path(expected, false)? == path)
        }
        RepairPathScope::DirectoryPrefix { path: prefix } => {
            let prefix = canonical_repair_path(prefix, false)?;
            Ok(path == prefix || path.starts_with(&format!("{prefix}/")))
        }
        RepairPathScope::GeneratedPattern {
            grammar_version,
            pattern,
        } => {
            if !generated_pattern_is_supported(pattern, *grammar_version) {
                return Err(RereviewFallbackReason::UnsupportedPathGrammar);
            }
            let pattern = canonical_repair_path(pattern, true)?;
            Ok(generated_pattern_matches(
                &pattern.split('/').collect::<Vec<_>>(),
                &path.split('/').collect::<Vec<_>>(),
            ))
        }
    }
}

fn path_resolves_within_repository(
    repository_root: &str,
    path: &str,
) -> Result<(), RereviewFallbackReason> {
    let canonical = canonical_repair_path(path, false)?;
    let root = std::fs::canonicalize(repository_root)
        .map_err(|_| RereviewFallbackReason::SymlinkEscape)?;
    let candidate = root.join(canonical.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut existing = candidate.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or(RereviewFallbackReason::SymlinkEscape)?;
    }
    let resolved =
        std::fs::canonicalize(existing).map_err(|_| RereviewFallbackReason::SymlinkEscape)?;
    if !resolved.starts_with(&root) {
        return Err(RereviewFallbackReason::SymlinkEscape);
    }
    Ok(())
}

fn plan_structure_hash(plan: &[EvidencePlanStep]) -> String {
    let structural_steps = plan
        .iter()
        .map(|step| {
            serde_json::json!({
                "id": step.id,
                "step": step.step,
                "dependsOn": step.depends_on,
                "acceptanceCriteria": step.acceptance_criteria,
                "runtimePaths": step.runtime_paths,
                "generatedArtifacts": step.generated_artifacts,
                "risks": step.risks,
                "requiresDesktopActivation": step.requires_desktop_activation,
                "editPaths": step.edit_paths,
            })
        })
        .collect::<Vec<_>>();
    canonical_hash(
        "KD4_PLAN_STRUCTURE_CANONICAL_V1",
        &serde_json::json!({ "steps": structural_steps }),
    )
}

fn path_scope_identifier(scope: &RepairPathScope) -> String {
    match scope {
        RepairPathScope::ExactFile { path } => format!("exact:{path}"),
        RepairPathScope::DirectoryPrefix { path } => format!("prefix:{path}"),
        RepairPathScope::GeneratedPattern {
            grammar_version,
            pattern,
        } => format!("generated:v{grammar_version}:{pattern}"),
    }
}

fn declared_repair_scopes(
    plan: &[EvidencePlanStep],
    paths: impl IntoIterator<Item = String>,
) -> (Vec<RepairPathScope>, Vec<RereviewFallbackReason>) {
    let mut scopes = Vec::new();
    let mut errors = BTreeSet::new();
    let candidates = paths.into_iter().chain(plan.iter().flat_map(|step| {
        step.edit_paths
            .iter()
            .chain(step.runtime_paths.iter())
            .chain(step.generated_artifacts.iter())
            .cloned()
    }));
    for candidate in candidates {
        let scope = if candidate.contains('*') {
            match canonical_repair_path(&candidate, true) {
                Ok(pattern)
                    if generated_pattern_is_supported(&pattern, REPAIR_PATH_GRAMMAR_VERSION) =>
                {
                    RepairPathScope::GeneratedPattern {
                        grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
                        pattern,
                    }
                }
                Ok(_) => {
                    errors.insert(RereviewFallbackReason::UnsupportedPathGrammar);
                    continue;
                }
                Err(reason) => {
                    errors.insert(reason);
                    continue;
                }
            }
        } else if candidate.ends_with('/') || candidate.ends_with('\\') {
            match canonical_repair_path(&candidate, false) {
                Ok(path) => RepairPathScope::DirectoryPrefix { path },
                Err(reason) => {
                    errors.insert(reason);
                    continue;
                }
            }
        } else {
            match canonical_repair_path(&candidate, false) {
                Ok(path) => RepairPathScope::ExactFile { path },
                Err(reason) => {
                    errors.insert(reason);
                    continue;
                }
            }
        };
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    scopes.sort_by_key(path_scope_identifier);
    (scopes, errors.into_iter().collect())
}

fn receipt_sequence(id: &str, prefix: &str) -> Option<u64> {
    id.strip_prefix(prefix)?.parse().ok()
}

fn current_repair_snapshot(
    document: &TaskEvidenceDocument,
    typed_mutation_identities: &[String],
) -> CurrentRepairSnapshot {
    let mut containment_errors = BTreeSet::new();
    let mut path_states = Vec::new();
    for snapshot in document
        .latest_file_hashes
        .values()
        .chain(document.latest_generated_artifact_hashes.values())
    {
        match canonical_repair_path(&snapshot.path, false) {
            Ok(path) => path_states.push(RepairPathState {
                path,
                exists: snapshot.exists,
                content_hash: snapshot.sha1.clone(),
            }),
            Err(reason) => {
                containment_errors.insert(reason);
            }
        }
        if snapshot.read_error.is_some() {
            containment_errors.insert(RereviewFallbackReason::UnrepresentableEvidenceChange);
        }
    }
    path_states.sort_by(|left, right| left.path.cmp(&right.path));
    path_states.dedup_by(|left, right| left.path == right.path);

    let (declared_path_scopes, scope_errors) = declared_repair_scopes(
        &document.plan,
        path_states.iter().map(|state| state.path.clone()),
    );
    containment_errors.extend(scope_errors);
    let implementation_surfaces = declared_path_scopes
        .iter()
        .map(|scope| StructuredContractSurface {
            kind: "path_scope".to_string(),
            owner: "task".to_string(),
            identifier: path_scope_identifier(scope),
        })
        .collect::<Vec<_>>();

    let mut command_receipts = document
        .command_receipts
        .iter()
        .filter_map(|receipt| {
            let sequence = receipt_sequence(&receipt.id, "command-")?;
            Some(RepairCommandDelta {
                sequence,
                receipt_id: receipt.id.clone(),
                command: receipt.command.join(" "),
                cwd: normalize_path_for_identity(Path::new(&receipt.cwd)),
                exit_code: Some(receipt.exit_code),
                timed_out: receipt.timed_out,
                implementation_identity: receipt.implementation_identity_hash.clone(),
            })
        })
        .collect::<Vec<_>>();
    if command_receipts.len() != document.command_receipts.len() {
        containment_errors.insert(RereviewFallbackReason::CommandLineageChanged);
    }
    command_receipts.sort_by_key(|receipt| receipt.sequence);

    let mut default_child_mutation_identities = document
        .completion_review_v2
        .as_ref()
        .into_iter()
        .flat_map(|ledger| ledger.attributed_workspace_events.iter())
        .map(|event| {
            serde_json::to_string(&serde_json::json!({
                "workspaceId": event.workspace_id,
                "epoch": event.epoch,
                "actorId": event.actor_id,
                "paths": event.paths,
            }))
            .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    default_child_mutation_identities.sort();
    default_child_mutation_identities.dedup();

    let mut typed_mutation_identities = typed_mutation_identities.to_vec();
    typed_mutation_identities.sort();
    typed_mutation_identities.dedup();
    let mut external_evidence_ids = document
        .external_evidence
        .iter()
        .filter(|receipt| receipt.task_epoch == document.evidence_epoch)
        .map(|receipt| receipt.id.clone())
        .collect::<Vec<_>>();
    external_evidence_ids.sort();
    external_evidence_ids.dedup();

    CurrentRepairSnapshot {
        repository_root: document.start.repository_root.clone(),
        path_states,
        command_receipts,
        plan_structure_hash: plan_structure_hash(&document.plan),
        declared_path_scopes,
        implementation_surfaces,
        default_child_mutation_identities,
        typed_mutation_identities,
        external_evidence_ids,
        containment_errors: containment_errors.into_iter().collect(),
    }
}

pub(crate) fn build_repair_baseline(
    dossier: &CompletionReviewDossier,
    findings: &[CompletionReviewFindingReceipt],
) -> Result<RepairBaseline, RereviewFallbackReason> {
    let active_requirement_ids = dossier
        .requirements
        .iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .map(|requirement| requirement.requirement_id.clone())
        .collect::<BTreeSet<_>>();
    let affected_requirement_ids =
        derive_affected_requirement_ids(&active_requirement_ids, findings)?;
    if !dossier
        .current_repair_snapshot
        .containment_errors
        .is_empty()
    {
        return Err(dossier.current_repair_snapshot.containment_errors[0].clone());
    }
    let command_bindings = dossier
        .current_repair_snapshot
        .command_receipts
        .iter()
        .map(|receipt| BaselineCommandBinding {
            sequence: receipt.sequence,
            receipt_id: receipt.receipt_id.clone(),
            implementation_identity: receipt.implementation_identity.clone(),
        })
        .collect::<Vec<_>>();
    let command_sequence_high_water_mark = command_bindings
        .iter()
        .map(|binding| binding.sequence)
        .max()
        .unwrap_or(0);
    Ok(RepairBaseline {
        path_states: dossier.current_repair_snapshot.path_states.clone(),
        command_sequence_high_water_mark,
        command_bindings,
        implementation_surfaces: dossier
            .current_repair_snapshot
            .implementation_surfaces
            .clone(),
        repair_scope: RepairScope {
            path_grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
            paths: dossier.current_repair_snapshot.declared_path_scopes.clone(),
            surfaces: dossier
                .current_repair_snapshot
                .implementation_surfaces
                .clone(),
            affected_requirement_ids,
        },
        source_ledger_hash: dossier.user_source_ledger_hash.clone(),
        requirement_manifest_hash: dossier.requirement_manifest_hash.clone(),
        plan_structure_hash: dossier.current_repair_snapshot.plan_structure_hash.clone(),
        default_child_mutation_identities: dossier
            .current_repair_snapshot
            .default_child_mutation_identities
            .clone(),
        typed_mutation_identities: dossier
            .current_repair_snapshot
            .typed_mutation_identities
            .clone(),
        external_evidence_ids: dossier
            .current_repair_snapshot
            .external_evidence_ids
            .clone(),
    })
}

fn derive_affected_requirement_ids(
    active_requirement_ids: &BTreeSet<String>,
    findings: &[CompletionReviewFindingReceipt],
) -> Result<Vec<String>, RereviewFallbackReason> {
    if findings.is_empty() {
        return Ok(active_requirement_ids.iter().cloned().collect());
    }
    let referenced = findings
        .iter()
        .flat_map(|finding| finding.requirement_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    if referenced.is_empty() || !referenced.is_subset(active_requirement_ids) {
        return Err(RereviewFallbackReason::RequirementManifestChanged);
    }
    Ok(referenced.into_iter().collect())
}

fn mutation_paths_are_attributable(identity: &str, scopes: &[RepairPathScope]) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(identity) else {
        return false;
    };
    let Some(paths) = value.get("paths").and_then(Value::as_array) else {
        return false;
    };
    !paths.is_empty()
        && paths.iter().all(|path| {
            path.as_str().is_some_and(|path| {
                scopes
                    .iter()
                    .any(|scope| repair_scope_matches(scope, path).unwrap_or(false))
            })
        })
}

fn build_rereview_input(
    baseline: Option<&RepairBaseline>,
    persisted_baseline_hash: Option<&str>,
    repair_instruction_hash: Option<&str>,
    original_findings: &[CompletionReviewFindingReceipt],
    current: &CurrentRepairSnapshot,
    candidate_implementation_identity: &str,
    source_ledger_hash: &str,
    requirement_manifest_hash: &str,
) -> Option<RereviewInput> {
    let repair_instruction_hash = repair_instruction_hash?.to_string();
    let mut reasons = current
        .containment_errors
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let Some(baseline) = baseline else {
        reasons.insert(RereviewFallbackReason::MissingBaseline);
        return Some(RereviewInput {
            input_mode: RereviewInputMode::FullFallback,
            baseline_hash: persisted_baseline_hash.map(str::to_string),
            delta_hash: None,
            fallback_reasons: reasons.into_iter().collect(),
            repair_instruction_hash,
            candidate_implementation_identity: candidate_implementation_identity.to_string(),
            delta: None,
        });
    };
    let computed_baseline_hash = repair_baseline_hash(baseline);
    if persisted_baseline_hash != Some(computed_baseline_hash.as_str()) {
        reasons.insert(RereviewFallbackReason::InvalidBaselineHash);
    }
    if baseline.repair_scope.path_grammar_version != REPAIR_PATH_GRAMMAR_VERSION {
        reasons.insert(RereviewFallbackReason::UnsupportedPathGrammar);
    }
    if baseline.source_ledger_hash != source_ledger_hash {
        reasons.insert(RereviewFallbackReason::SourceIdentityChanged);
    }
    if baseline.requirement_manifest_hash != requirement_manifest_hash {
        reasons.insert(RereviewFallbackReason::RequirementManifestChanged);
    }
    if baseline.plan_structure_hash != current.plan_structure_hash {
        reasons.insert(RereviewFallbackReason::PlanStructureChanged);
    }

    let before_paths = baseline
        .path_states
        .iter()
        .map(|state| (state.path.as_str(), state))
        .collect::<BTreeMap<_, _>>();
    let after_paths = current
        .path_states
        .iter()
        .map(|state| (state.path.as_str(), state))
        .collect::<BTreeMap<_, _>>();
    let all_paths = before_paths
        .keys()
        .chain(after_paths.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut path_changes = Vec::new();
    for path in all_paths {
        let before = before_paths.get(path).copied();
        let after = after_paths.get(path).copied();
        if before == after {
            continue;
        }
        match baseline
            .repair_scope
            .paths
            .iter()
            .map(|scope| repair_scope_matches(scope, path))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(matches) if matches.iter().any(|matches| *matches) => {}
            Ok(_) => {
                reasons.insert(RereviewFallbackReason::PathOutsideScope);
            }
            Err(reason) => {
                reasons.insert(reason);
            }
        }
        if let Err(reason) = path_resolves_within_repository(&current.repository_root, path) {
            reasons.insert(reason);
        }
        let before_exists = before.is_some_and(|state| state.exists);
        let after_exists = after.is_some_and(|state| state.exists);
        if !before_exists && !after_exists {
            continue;
        }
        let before_hash = before.and_then(|state| state.content_hash.clone());
        let after_hash = after.and_then(|state| state.content_hash.clone());
        let change = match (before_exists, after_exists) {
            (false, true) => RepairPathChangeKind::Added,
            (true, false) => RepairPathChangeKind::Removed,
            (true, true) => RepairPathChangeKind::Modified,
            (false, false) => unreachable!(),
        };
        path_changes.push(RepairPathChange {
            path: path.to_string(),
            change,
            before_exists,
            before_hash,
            after_exists,
            after_hash,
        });
    }

    let current_commands = current
        .command_receipts
        .iter()
        .map(|receipt| (receipt.sequence, receipt))
        .collect::<BTreeMap<_, _>>();
    let mut invalidated_command_receipts = Vec::new();
    for binding in &baseline.command_bindings {
        let Some(current_receipt) = current_commands.get(&binding.sequence) else {
            reasons.insert(RereviewFallbackReason::CommandLineageChanged);
            continue;
        };
        if current_receipt.receipt_id != binding.receipt_id {
            reasons.insert(RereviewFallbackReason::CommandLineageChanged);
        }
        if binding.implementation_identity.as_deref() != Some(candidate_implementation_identity) {
            invalidated_command_receipts.push(InvalidatedCommandReceipt {
                sequence: binding.sequence,
                receipt_id: binding.receipt_id.clone(),
                reason: "implementation_identity_binding_mismatch".to_string(),
            });
        }
    }
    let mut new_command_receipts = current
        .command_receipts
        .iter()
        .filter(|receipt| receipt.sequence > baseline.command_sequence_high_water_mark)
        .cloned()
        .collect::<Vec<_>>();
    new_command_receipts.sort_by_key(|receipt| receipt.sequence);

    let baseline_children = baseline
        .default_child_mutation_identities
        .iter()
        .collect::<BTreeSet<_>>();
    if current
        .default_child_mutation_identities
        .iter()
        .filter(|identity| !baseline_children.contains(identity))
        .any(|identity| !mutation_paths_are_attributable(identity, &baseline.repair_scope.paths))
    {
        reasons.insert(RereviewFallbackReason::UnattributedMutation);
    }
    let baseline_typed = baseline
        .typed_mutation_identities
        .iter()
        .collect::<BTreeSet<_>>();
    if current
        .typed_mutation_identities
        .iter()
        .filter(|identity| !baseline_typed.contains(identity))
        .any(|identity| !mutation_paths_are_attributable(identity, &baseline.repair_scope.paths))
    {
        reasons.insert(RereviewFallbackReason::UnattributedMutation);
    }
    if baseline.external_evidence_ids != current.external_evidence_ids {
        reasons.insert(RereviewFallbackReason::UnrepresentableEvidenceChange);
    }

    let permitted_surfaces = baseline
        .repair_scope
        .surfaces
        .iter()
        .collect::<BTreeSet<_>>();
    let baseline_surfaces = baseline
        .implementation_surfaces
        .iter()
        .collect::<BTreeSet<_>>();
    let mut newly_realized_surfaces = current
        .implementation_surfaces
        .iter()
        .filter(|surface| !baseline_surfaces.contains(surface))
        .cloned()
        .collect::<Vec<_>>();
    newly_realized_surfaces.sort();
    newly_realized_surfaces.dedup();
    if newly_realized_surfaces
        .iter()
        .any(|surface| !permitted_surfaces.contains(surface))
    {
        reasons.insert(RereviewFallbackReason::ContractSurfaceOutsideScope);
    }

    if !reasons.is_empty() {
        return Some(RereviewInput {
            input_mode: RereviewInputMode::FullFallback,
            baseline_hash: Some(computed_baseline_hash),
            delta_hash: None,
            fallback_reasons: reasons.into_iter().collect(),
            repair_instruction_hash,
            candidate_implementation_identity: candidate_implementation_identity.to_string(),
            delta: None,
        });
    }
    let delta = RepairDelta {
        original_findings: original_findings.to_vec(),
        required_disposition_finding_ids: original_findings
            .iter()
            .map(|finding| finding.finding_id.clone())
            .collect(),
        repair_instruction_hash: repair_instruction_hash.clone(),
        baseline_hash: computed_baseline_hash.clone(),
        candidate_implementation_identity: candidate_implementation_identity.to_string(),
        path_changes,
        new_command_receipts,
        invalidated_command_receipts,
        affected_requirement_ids: baseline.repair_scope.affected_requirement_ids.clone(),
        newly_realized_surfaces,
    };
    Some(RereviewInput {
        input_mode: RereviewInputMode::Delta,
        baseline_hash: Some(computed_baseline_hash),
        delta_hash: Some(repair_delta_hash(&delta)),
        fallback_reasons: Vec::new(),
        repair_instruction_hash,
        candidate_implementation_identity: candidate_implementation_identity.to_string(),
        delta: Some(delta),
    })
}

pub(crate) fn repair_baseline_hash(baseline: &RepairBaseline) -> String {
    canonical_hash(
        REPAIR_BASELINE_CANONICAL_FORMAT,
        &serde_json::to_value(baseline).unwrap_or(Value::Null),
    )
}

pub(crate) fn repair_delta_hash(delta: &RepairDelta) -> String {
    canonical_hash(
        REPAIR_DELTA_CANONICAL_FORMAT,
        &serde_json::to_value(delta).unwrap_or(Value::Null),
    )
}

fn rereview_audit_hash(input: &RereviewInput) -> String {
    canonical_hash(
        REREVIEW_AUDIT_CANONICAL_FORMAT,
        &serde_json::to_value(input).unwrap_or(Value::Null),
    )
}

fn repair_instruction_matches_baseline(
    instruction: &str,
    baseline: &RepairBaseline,
    baseline_hash: &str,
) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(instruction) else {
        return false;
    };
    value.get("repair_baseline_hash").and_then(Value::as_str) == Some(baseline_hash)
        && value.get("declared_repair_scope")
            == serde_json::to_value(&baseline.repair_scope).ok().as_ref()
}

fn validate_rereview_input(input: &RereviewInput, dossier: &CompletionReviewDossier) -> bool {
    if input.repair_instruction_hash
        != dossier
            .initial_repair_instruction_hash
            .as_deref()
            .unwrap_or_default()
        || input.candidate_implementation_identity != dossier.implementation_identity_hash
    {
        return false;
    }
    match input.input_mode {
        RereviewInputMode::Delta => {
            let (Some(baseline), Some(baseline_hash), Some(delta), Some(delta_hash)) = (
                dossier.initial_repair_baseline.as_ref(),
                input.baseline_hash.as_deref(),
                input.delta.as_ref(),
                input.delta_hash.as_deref(),
            ) else {
                return false;
            };
            input.fallback_reasons.is_empty()
                && repair_baseline_hash(baseline) == baseline_hash
                && repair_delta_hash(delta) == delta_hash
                && delta.baseline_hash == baseline_hash
                && delta.repair_instruction_hash == input.repair_instruction_hash
                && delta.candidate_implementation_identity
                    == input.candidate_implementation_identity
                && validate_repair_delta_contents(delta, baseline, &dossier.original_findings)
                    .is_ok()
        }
        RereviewInputMode::FullFallback => {
            !input.fallback_reasons.is_empty()
                && input
                    .fallback_reasons
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                && input.delta_hash.is_none()
                && input.delta.is_none()
                && input.baseline_hash.as_deref() == dossier.initial_repair_baseline_hash.as_deref()
        }
    }
}

async fn persist_document_with_permit(
    path: PathBuf,
    document: TaskEvidenceDocument,
    permit: tokio::sync::OwnedSemaphorePermit,
    last_persisted_revision: Arc<AtomicU64>,
    test_control: Option<PersistenceTestControl>,
) -> (PersistOutcome, Option<tokio::sync::OwnedSemaphorePermit>) {
    let bytes = match serde_json::to_vec_pretty(&document) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("failed to serialize KD4 task evidence: {err}");
            return (PersistOutcome::Failed, Some(permit));
        }
    };
    match tokio::task::spawn_blocking(move || {
        if let Some(control) = test_control.as_ref()
            && let Some((started, release)) = control
                .before_next_write
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            started.wait();
            release.wait();
        }
        let last_revision = last_persisted_revision.load(Ordering::Acquire);
        let outcome = if last_revision > document.revision {
            PersistOutcome::Superseded
        } else if last_revision == document.revision && last_revision != 0 {
            PersistOutcome::Persisted
        } else {
            let write_result = if test_control
                .as_ref()
                .is_some_and(|control| control.fail_writes.load(Ordering::Acquire))
            {
                Err(io::Error::other(
                    "injected task-evidence persistence failure",
                ))
            } else {
                atomic_write_evidence(&path, &bytes)
            };
            match write_result {
                Ok(()) => {
                    last_persisted_revision.store(document.revision, Ordering::Release);
                    PersistOutcome::Persisted
                }
                Err(err) => {
                    warn!("failed to persist KD4 task evidence: {err}");
                    PersistOutcome::Failed
                }
            }
        };
        (outcome, permit)
    })
    .await
    {
        Ok((outcome, permit)) => (outcome, Some(permit)),
        Err(err) => {
            warn!("KD4 task-evidence persistence task failed: {err}");
            (PersistOutcome::Failed, None)
        }
    }
}

async fn delete_external_artifact_owned(
    codex_home: Option<&Path>,
    thread_id: Option<&str>,
    artifact_id: Option<&str>,
) {
    let (Some(artifact_id), Some(codex_home), Some(thread_id)) =
        (artifact_id, codex_home, thread_id)
    else {
        return;
    };
    if let Err(err) = crate::tools::command_output_artifact::delete_evidence_artifact(
        codex_home,
        thread_id,
        artifact_id,
    )
    .await
    {
        warn!("failed to delete external evidence artifact {artifact_id}: {err}");
    }
}

struct ExternalEvidenceMetadata {
    producer: String,
    producer_schema_version: u32,
    provider_snapshot: Option<String>,
    payload_completeness: EvidenceCompleteness,
    truncated: bool,
    approximate: bool,
    limitations: Vec<String>,
}

fn extract_external_evidence_metadata(
    result: &CallToolResult,
) -> Result<Option<ExternalEvidenceMetadata>, &'static str> {
    let Some(structured) = result.structured_content.as_ref() else {
        return Ok(None);
    };
    let Some(evidence_meta) = structured.get("evidenceMeta") else {
        return Ok(None);
    };
    let Some(meta) = evidence_meta.as_object() else {
        return Err("MCP evidenceMeta is malformed and was ignored");
    };
    let Some(schema_version) = meta.get("schemaVersion").and_then(Value::as_u64) else {
        return Err("MCP evidenceMeta schemaVersion is malformed and was ignored");
    };
    if schema_version != 1 {
        return Err("MCP evidenceMeta schemaVersion is unsupported and was ignored");
    }
    let Some(producer) = meta.get("producer").and_then(Value::as_str) else {
        return Err("MCP evidenceMeta producer is malformed and was ignored");
    };
    if producer.trim().is_empty() {
        return Err("MCP evidenceMeta producer is malformed and was ignored");
    }
    let Some(evidence_bearing) = meta.get("evidenceBearing").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta evidenceBearing is malformed and was ignored");
    };
    if !evidence_bearing {
        return Ok(None);
    }
    if meta
        .get("operation")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("MCP evidenceMeta operation is malformed and was ignored");
    }
    let payload_completeness = match meta.get("payloadCompleteness").and_then(Value::as_str) {
        Some("complete") => EvidenceCompleteness::Complete,
        Some("partial") => EvidenceCompleteness::Partial,
        Some("unknown") => EvidenceCompleteness::Unknown,
        _ => {
            return Err("MCP evidenceMeta payloadCompleteness is malformed and was ignored");
        }
    };
    let Some(truncated) = meta.get("truncated").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta truncated flag is malformed and was ignored");
    };
    if payload_completeness == EvidenceCompleteness::Complete && truncated {
        return Err("MCP evidenceMeta complete payload cannot be truncated and was ignored");
    }
    let Some(approximate) = meta.get("approximate").and_then(Value::as_bool) else {
        return Err("MCP evidenceMeta approximate flag is malformed and was ignored");
    };
    let Some(limitations) = meta.get("limitations").and_then(Value::as_array) else {
        return Err("MCP evidenceMeta limitations are malformed and were ignored");
    };
    let Some(limitations) = limitations
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
    else {
        return Err("MCP evidenceMeta limitations are malformed and were ignored");
    };
    let provider_snapshot = match meta.get("snapshot") {
        // Keep ingress tolerant for providers deployed before snapshot became
        // explicitly required by the producer contract.
        None | Some(Value::Null) => None,
        Some(Value::String(snapshot)) => Some(snapshot.clone()),
        Some(_) => return Err("MCP evidenceMeta snapshot is malformed and was ignored"),
    };
    Ok(Some(ExternalEvidenceMetadata {
        producer: producer.to_string(),
        producer_schema_version: schema_version as u32,
        provider_snapshot,
        payload_completeness,
        truncated,
        approximate,
        limitations,
    }))
}

fn canonical_mcp_result_payload(result: &CallToolResult) -> Value {
    canonicalize_json_value(serde_json::json!({
        "content": result.content,
        "structuredContent": result.structured_content,
        "isError": result.is_error,
    }))
}

fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json_value(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => {
            Value::Array(values.into_iter().map(canonicalize_json_value).collect())
        }
        other => other,
    }
}

const fn evidence_completeness_name(completeness: EvidenceCompleteness) -> &'static str {
    match completeness {
        EvidenceCompleteness::Complete => "complete",
        EvidenceCompleteness::Partial => "partial",
        EvidenceCompleteness::Unknown => "unknown",
    }
}

fn encode_external_evidence_artifact(canonical_bytes: &[u8]) -> Option<Vec<u8>> {
    let canonical = std::str::from_utf8(canonical_bytes).ok()?;
    let mut encoded = Vec::with_capacity(canonical_bytes.len() + 256);
    encoded.extend_from_slice(EXTERNAL_EVIDENCE_ARTIFACT_HEADER.as_bytes());
    let mut start = 0;
    while start < canonical.len() {
        let mut end = (start + EXTERNAL_EVIDENCE_ARTIFACT_CHUNK_BYTES).min(canonical.len());
        while !canonical.is_char_boundary(end) {
            end -= 1;
        }
        let line = Value::String(canonical[start..end].to_string()).to_string();
        encoded.extend_from_slice(line.as_bytes());
        encoded.push(b'\n');
        start = end;
    }
    Some(encoded)
}

fn workspace_root_fingerprint(start: &TaskStartState) -> String {
    let identity = canonicalize_json_value(serde_json::json!({
        "repositoryRoot": start.repository_root,
        "repositoryUrl": start.repository_url,
    }));
    let serialized = identity.to_string();
    format!("{:x}", Sha256::digest(serialized.as_bytes()))
}

enum ExistingDocument {
    Missing,
    Loaded {
        document: Box<TaskEvidenceDocument>,
        legacy_completion_model: bool,
    },
    NewerSchema {
        schema_version: u64,
    },
    Rejected {
        kind: &'static str,
        reason: String,
    },
}

async fn load_existing_document(
    path: &Path,
    expected_thread_id: &str,
    expected_repository_root: &Path,
) -> ExistingDocument {
    load_existing_document_with_supported_version(
        path,
        expected_thread_id,
        expected_repository_root,
        TASK_EVIDENCE_SCHEMA_VERSION,
    )
    .await
}

async fn load_existing_document_with_supported_version(
    path: &Path,
    expected_thread_id: &str,
    expected_repository_root: &Path,
    max_supported_schema_version: u32,
) -> ExistingDocument {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ExistingDocument::Missing,
        Err(err) => {
            return ExistingDocument::Rejected {
                kind: "unreadable",
                reason: format!("could not read evidence: {err}"),
            };
        }
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(err) => {
            return ExistingDocument::Rejected {
                kind: "corrupt",
                reason: format!("invalid JSON: {err}"),
            };
        }
    };
    let schema_version = match value.get("schema_version").and_then(Value::as_u64) {
        Some(schema_version) => schema_version,
        None => {
            return ExistingDocument::Rejected {
                kind: "incompatible",
                reason: "missing numeric schema_version".to_string(),
            };
        }
    };
    if schema_version > u64::from(max_supported_schema_version) {
        return ExistingDocument::NewerSchema { schema_version };
    }
    if schema_version == 0 {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: format!("unsupported schema version {schema_version}"),
        };
    }
    let schema_version = schema_version as u32;
    if schema_version == TASK_EVIDENCE_SCHEMA_VERSION
        && !value
            .get("source_classification_cache")
            .is_some_and(Value::is_array)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: "V6 requires an array source_classification_cache".to_string(),
        };
    }
    let legacy_completion_model = schema_version < TASK_EVIDENCE_COMPLETION_MODEL_VERSION
        || uses_retired_v3_completion_shape(schema_version, &value);
    let document = match serde_json::from_value::<TaskEvidenceDocument>(value) {
        Ok(document) => document,
        Err(err) => {
            return ExistingDocument::Rejected {
                kind: "corrupt",
                reason: format!("schema-valid JSON could not be decoded: {err}"),
            };
        }
    };
    if document.thread_id != expected_thread_id {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: "thread id does not match the requested task".to_string(),
        };
    }
    if !recorded_repository_root_matches(&document.start.repository_root, expected_repository_root)
    {
        return ExistingDocument::Rejected {
            kind: "incompatible",
            reason: "repository root does not match the requested checkout".to_string(),
        };
    }
    if matches!(
        schema_version,
        FROZEN_TASK_EVIDENCE_V5_SCHEMA_VERSION | TASK_EVIDENCE_SCHEMA_VERSION
    ) && let Err(reason) = validate_v5_completion_review(&document)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: format!("invalid V5 completion-review lineage: {reason}"),
        };
    }
    if schema_version == TASK_EVIDENCE_SCHEMA_VERSION
        && let Err(reason) = validate_v6_source_classification_state(&document)
    {
        return ExistingDocument::Rejected {
            kind: "corrupt",
            reason: format!("invalid V6 source-classification state: {reason}"),
        };
    }
    ExistingDocument::Loaded {
        document: Box::new(document),
        legacy_completion_model,
    }
}

fn validate_v6_source_classification_state(document: &TaskEvidenceDocument) -> Result<(), String> {
    if document.source_classification_cache
        != canonical_source_classification_cache(document.source_classification_cache.clone())
    {
        return Err("cache is not canonical, sorted, and unique".to_string());
    }
    let Some(ledger) = document.completion_review_v2.as_ref() else {
        return Ok(());
    };
    for revision in &ledger.mapping_revisions {
        match (
            revision.source_classification_contract_version.as_deref(),
            revision.relationship_resolver_contract_version.as_deref(),
        ) {
            (None, None) => {}
            (Some(source_version), Some(resolver_version))
                if !source_version.trim().is_empty() && !resolver_version.trim().is_empty() => {}
            _ => return Err("mapping revision contract versions are incomplete".to_string()),
        }
    }
    Ok(())
}

fn requirement_supersession_is_acyclic(requirements: &[RequirementRecord]) -> bool {
    let by_id = requirements
        .iter()
        .map(|requirement| (requirement.requirement_id.as_str(), requirement))
        .collect::<BTreeMap<_, _>>();
    requirements.iter().all(|requirement| {
        let mut visited = BTreeSet::new();
        let mut current = Some(requirement.requirement_id.as_str());
        while let Some(requirement_id) = current {
            if !visited.insert(requirement_id) {
                return false;
            }
            let Some(requirement) = by_id.get(requirement_id) else {
                return false;
            };
            current = requirement.superseded_by.as_deref();
        }
        true
    })
}

fn validate_v5_completion_review(document: &TaskEvidenceDocument) -> Result<(), String> {
    let ledger = document
        .completion_review_v2
        .as_ref()
        .ok_or_else(|| "missing completion_review_v2 ledger".to_string())?;
    if ledger.root_task_id != document.thread_id || ledger.completion_epoch == 0 {
        return Err("root task or completion epoch is inconsistent".to_string());
    }
    let mut source_ordinals = BTreeSet::new();
    for (source_id, source) in &ledger.source_records {
        let expected_content_hash = user_source_content_hash(
            source.source_kind,
            &source.exact_material,
            source.availability,
        );
        let expected_source_id = deterministic_source_id(
            &ledger.root_task_id,
            source.completion_epoch,
            &source.message_id,
            source.content_ordinal,
            &expected_content_hash,
        );
        if source_id != &source.source_id
            || source_id != &expected_source_id
            || source.content_hash != expected_content_hash
            || source.completion_epoch == 0
            || source.completion_epoch > ledger.completion_epoch
            || !source_ordinals.insert((source.completion_epoch, source.source_ordinal))
        {
            return Err(
                "source record identity, content hash, ordinal, or epoch is inconsistent"
                    .to_string(),
            );
        }
    }

    let mut mapping_keys = BTreeSet::new();
    for mapping in &ledger.mapping_revisions {
        let Some(source) = ledger.source_records.get(&mapping.source_id) else {
            return Err("source mapping references an unknown source".to_string());
        };
        if source.completion_epoch != mapping.completion_epoch
            || !mapping_keys.insert((
                mapping.completion_epoch,
                mapping.manifest_revision,
                mapping.source_id.clone(),
            ))
        {
            return Err("source mapping revision is duplicated or cross-epoch".to_string());
        }
        if matches!(
            &mapping.mapping,
            SourceMapping::NonRequirement { reason }
                | SourceMapping::SupersededContext { reason }
                if reason.trim().is_empty()
        ) {
            return Err("source mapping reason is blank".to_string());
        }
    }

    let mut manifest_keys = BTreeSet::new();
    let mut all_requirement_ids = BTreeSet::new();
    for manifest in &ledger.manifest_snapshots {
        if !manifest_keys.insert((manifest.completion_epoch, manifest.manifest_revision))
            || manifest.manifest_hash
                != requirement_manifest_hash(manifest.manifest_revision, &manifest.requirements)
        {
            return Err(
                "requirement manifest revision is duplicated or has a bad hash".to_string(),
            );
        }
        let mut ids = BTreeSet::new();
        for requirement in &manifest.requirements {
            let Some(source) = ledger.source_records.get(&requirement.source_id) else {
                return Err("requirement references an unknown source".to_string());
            };
            if source.completion_epoch != manifest.completion_epoch
                || source.content_hash != requirement.source_content_hash
                || material_for_span(source, &requirement.source_span).as_deref()
                    != Some(requirement.exact_material.as_str())
                || requirement.requirement_id
                    != deterministic_requirement_id(source, &requirement.source_span)
                || !ids.insert(requirement.requirement_id.clone())
            {
                return Err("requirement provenance, span, or identity is inconsistent".to_string());
            }
            match requirement.status {
                RequirementStatus::Active | RequirementStatus::Withdrawn
                    if requirement.superseded_by.is_some() =>
                {
                    return Err("active or withdrawn requirement has superseded_by".to_string());
                }
                RequirementStatus::Superseded if requirement.superseded_by.is_none() => {
                    return Err("superseded requirement lacks superseded_by".to_string());
                }
                _ => {}
            }
            all_requirement_ids.insert(requirement.requirement_id.clone());
        }
        if manifest.requirements.iter().any(|requirement| {
            requirement
                .superseded_by
                .as_ref()
                .is_some_and(|requirement_id| !ids.contains(requirement_id))
        }) {
            return Err("superseded_by points outside its manifest".to_string());
        }
        if !requirement_supersession_is_acyclic(&manifest.requirements) {
            return Err("requirement supersession graph contains a cycle".to_string());
        }
    }

    if ledger.manifest_revision > 0 {
        let active_manifest = active_manifest(ledger)
            .ok_or_else(|| "active manifest revision is missing".to_string())?;
        let active_mappings = active_source_mappings(ledger);
        let active_source_ids = ledger
            .source_records
            .values()
            .filter(|source| source.completion_epoch == ledger.completion_epoch)
            .map(|source| source.source_id.as_str())
            .collect::<BTreeSet<_>>();
        if active_mappings
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != active_source_ids
        {
            return Err("active source mapping set is incomplete".to_string());
        }
        let ids_by_source = active_manifest.requirements.iter().fold(
            BTreeMap::<String, Vec<String>>::new(),
            |mut result, requirement| {
                result
                    .entry(requirement.source_id.clone())
                    .or_default()
                    .push(requirement.requirement_id.clone());
                result
            },
        );
        for (source_id, mapping) in active_mappings {
            if let SourceMapping::RequirementBearing {
                mut requirement_ids,
            } = mapping
            {
                requirement_ids.sort();
                let mut expected = ids_by_source.get(&source_id).cloned().unwrap_or_default();
                expected.sort();
                if requirement_ids != expected {
                    return Err("active source mapping is not two-way consistent".to_string());
                }
            } else if ids_by_source.contains_key(&source_id) {
                return Err("requirement-bearing source has a non-requirement mapping".to_string());
            }
        }
    }

    validate_v5_revision_snapshots(ledger)?;
    validate_v5_review_receipts(document, ledger)?;
    Ok(())
}

fn validate_v5_revision_snapshots(ledger: &CompletionReviewLedgerV2) -> Result<(), String> {
    let mut revisions_by_epoch = BTreeMap::<u64, BTreeSet<u64>>::new();
    for manifest in &ledger.manifest_snapshots {
        if manifest.completion_epoch == 0
            || manifest.completion_epoch > ledger.completion_epoch
            || manifest.manifest_revision == 0
        {
            return Err("manifest snapshot epoch or revision is invalid".to_string());
        }
        revisions_by_epoch
            .entry(manifest.completion_epoch)
            .or_default()
            .insert(manifest.manifest_revision);
    }
    for (epoch, revisions) in &revisions_by_epoch {
        let max_revision = revisions.iter().next_back().copied().unwrap_or_default();
        let expected = (1..=max_revision).collect::<BTreeSet<_>>();
        if revisions != &expected {
            return Err("manifest revisions are not contiguous within an epoch".to_string());
        }
        for revision in revisions {
            let manifest = ledger
                .manifest_snapshots
                .iter()
                .find(|manifest| {
                    manifest.completion_epoch == *epoch && manifest.manifest_revision == *revision
                })
                .ok_or_else(|| "manifest revision is missing".to_string())?;
            let mappings = source_mappings_for(ledger, *epoch, *revision);
            let expected_sources = ledger
                .source_records
                .values()
                .filter(|source| {
                    source.completion_epoch == *epoch
                        && source.introduced_manifest_revision > 0
                        && source.introduced_manifest_revision <= *revision
                })
                .map(|source| source.source_id.as_str())
                .collect::<BTreeSet<_>>();
            if mappings.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_sources {
                return Err("historical source mapping set is incomplete".to_string());
            }
            let ids_by_source = manifest.requirements.iter().fold(
                BTreeMap::<String, Vec<String>>::new(),
                |mut result, requirement| {
                    result
                        .entry(requirement.source_id.clone())
                        .or_default()
                        .push(requirement.requirement_id.clone());
                    result
                },
            );
            for (source_id, mapping) in mappings {
                let mut expected_ids = ids_by_source.get(&source_id).cloned().unwrap_or_default();
                expected_ids.sort();
                match mapping {
                    SourceMapping::RequirementBearing {
                        mut requirement_ids,
                    } => {
                        requirement_ids.sort();
                        requirement_ids.dedup();
                        if requirement_ids != expected_ids {
                            return Err(
                                "historical source mapping is not two-way consistent".to_string()
                            );
                        }
                    }
                    SourceMapping::PendingClassification
                    | SourceMapping::NonRequirement { .. }
                    | SourceMapping::SupersededContext { .. }
                    | SourceMapping::UnavailableOrTruncated => {
                        if !expected_ids.is_empty() {
                            return Err(
                                "historical requirement-bearing source has an incompatible mapping"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
        }
    }
    if ledger.manifest_revision > 0
        && revisions_by_epoch
            .get(&ledger.completion_epoch)
            .and_then(|revisions| revisions.iter().next_back().copied())
            != Some(ledger.manifest_revision)
    {
        return Err("active manifest revision is not the latest revision in its epoch".to_string());
    }
    for source in ledger.source_records.values() {
        if source.introduced_manifest_revision == 0
            || revisions_by_epoch
                .get(&source.completion_epoch)
                .is_none_or(|revisions| !revisions.contains(&source.introduced_manifest_revision))
        {
            return Err("source introduction revision is missing or invalid".to_string());
        }
    }
    let max_source_ordinal = ledger
        .source_records
        .values()
        .map(|source| source.source_ordinal)
        .max()
        .unwrap_or_default();
    if ledger.next_source_ordinal <= max_source_ordinal {
        return Err("next source ordinal can reuse a persisted source identity".to_string());
    }
    Ok(())
}

fn parse_review_coordinates(review_id: &str) -> Option<(u64, u64, u64)> {
    let mut fields = review_id.strip_prefix("review-")?.split('-');
    let epoch = fields.next()?.parse().ok()?;
    let revision = fields.next()?.parse().ok()?;
    let sequence = fields.next()?.parse().ok()?;
    fields
        .next()
        .is_none()
        .then_some((epoch, revision, sequence))
}

fn receipt_identity_matches(
    left: &CompletionReviewReceiptV2,
    right: &CompletionReviewReceiptV2,
) -> bool {
    left.candidate_mutation_revision == right.candidate_mutation_revision
        && left.candidate_hash == right.candidate_hash
        && left.implementation_identity_hash == right.implementation_identity_hash
        && left.dossier_snapshot_id == right.dossier_snapshot_id
        && left.user_source_ledger_hash == right.user_source_ledger_hash
        && left.requirement_manifest_hash == right.requirement_manifest_hash
}

fn manifest_gap_supersession_is_valid(
    receipt: &CompletionReviewReceiptV2,
    superseded: &CompletionReviewReceiptV2,
) -> bool {
    let Some((epoch, revision, _)) = parse_review_coordinates(&receipt.review_id) else {
        return false;
    };
    let Some((superseded_epoch, superseded_revision, _)) =
        parse_review_coordinates(&superseded.review_id)
    else {
        return false;
    };
    receipt.attempt_kind == CompletionReviewAttemptKind::InitialReview
        && matches!(
            superseded.attempt_kind,
            CompletionReviewAttemptKind::InitialReview | CompletionReviewAttemptKind::Rereview
        )
        && superseded.infrastructure_outcome == "ok"
        && !superseded.review_clean
        && superseded.terminal_outcome.is_none()
        && !superseded.manifest_gaps.is_empty()
        && epoch == superseded_epoch
        && superseded_revision < revision
}

fn is_strictly_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn structured_surface_is_canonical(surface: &StructuredContractSurface) -> bool {
    !surface.kind.trim().is_empty()
        && surface.kind == surface.kind.trim()
        && !surface.owner.trim().is_empty()
        && surface.owner == surface.owner.trim()
        && !surface.identifier.trim().is_empty()
        && surface.identifier == surface.identifier.trim()
}

fn validate_persisted_repair_baseline(
    baseline: &RepairBaseline,
    initial_receipt: &CompletionReviewReceiptV2,
    manifest: &RequirementManifestSnapshot,
) -> Result<(), String> {
    if baseline.source_ledger_hash != initial_receipt.user_source_ledger_hash
        || baseline.requirement_manifest_hash != initial_receipt.requirement_manifest_hash
        || baseline.plan_structure_hash.trim().is_empty()
        || baseline.repair_scope.path_grammar_version != REPAIR_PATH_GRAMMAR_VERSION
    {
        return Err("repair baseline identity or grammar is invalid".to_string());
    }

    if baseline
        .path_states
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
        || baseline.path_states.iter().any(|state| {
            canonical_repair_path(&state.path, false).as_deref() != Ok(state.path.as_str())
                || state.exists != state.content_hash.is_some()
                || state.content_hash.as_deref().is_some_and(str::is_empty)
        })
    {
        return Err("repair baseline path states are not canonical".to_string());
    }

    let scope_ids = baseline
        .repair_scope
        .paths
        .iter()
        .map(path_scope_identifier)
        .collect::<Vec<_>>();
    if !is_strictly_sorted_unique(&scope_ids)
        || baseline.repair_scope.paths.iter().any(|scope| match scope {
            RepairPathScope::ExactFile { path } | RepairPathScope::DirectoryPrefix { path } => {
                canonical_repair_path(path, false).as_deref() != Ok(path.as_str())
            }
            RepairPathScope::GeneratedPattern {
                grammar_version,
                pattern,
            } => {
                !generated_pattern_is_supported(pattern, *grammar_version)
                    || canonical_repair_path(pattern, true).as_deref() != Ok(pattern.as_str())
            }
        })
    {
        return Err("repair baseline path scope is not canonical".to_string());
    }

    if !is_strictly_sorted_unique(&baseline.implementation_surfaces)
        || !is_strictly_sorted_unique(&baseline.repair_scope.surfaces)
        || baseline
            .implementation_surfaces
            .iter()
            .chain(baseline.repair_scope.surfaces.iter())
            .any(|surface| !structured_surface_is_canonical(surface))
        || baseline
            .implementation_surfaces
            .iter()
            .any(|surface| !baseline.repair_scope.surfaces.contains(surface))
    {
        return Err("repair baseline contract surfaces are not canonical".to_string());
    }

    if baseline
        .command_bindings
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
        || baseline
            .command_bindings
            .iter()
            .any(|binding| binding.receipt_id.trim().is_empty())
        || baseline
            .command_bindings
            .iter()
            .map(|binding| binding.receipt_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != baseline.command_bindings.len()
        || baseline.command_sequence_high_water_mark
            != baseline
                .command_bindings
                .last()
                .map(|binding| binding.sequence)
                .unwrap_or(0)
    {
        return Err("repair baseline command sequence is not canonical".to_string());
    }

    for identities in [
        &baseline.default_child_mutation_identities,
        &baseline.typed_mutation_identities,
        &baseline.external_evidence_ids,
    ] {
        if !is_strictly_sorted_unique(identities)
            || identities.iter().any(|identity| identity.trim().is_empty())
        {
            return Err("repair baseline evidence identities are not canonical".to_string());
        }
    }

    let active_requirement_ids = manifest
        .requirements
        .iter()
        .filter(|requirement| requirement.status == RequirementStatus::Active)
        .map(|requirement| requirement.requirement_id.clone())
        .collect::<BTreeSet<_>>();
    let expected_affected =
        derive_affected_requirement_ids(&active_requirement_ids, &initial_receipt.findings)
            .map_err(|_| {
                "repair baseline references an inactive or unknown requirement".to_string()
            })?;
    if baseline.repair_scope.affected_requirement_ids != expected_affected {
        return Err("repair baseline affected requirements are invalid".to_string());
    }
    Ok(())
}

fn validate_repair_delta_contents(
    delta: &RepairDelta,
    baseline: &RepairBaseline,
    original_findings: &[CompletionReviewFindingReceipt],
) -> Result<(), String> {
    let expected_disposition_ids = original_findings
        .iter()
        .map(|finding| finding.finding_id.clone())
        .collect::<Vec<_>>();
    if delta.original_findings != original_findings
        || delta.required_disposition_finding_ids != expected_disposition_ids
        || delta.affected_requirement_ids != baseline.repair_scope.affected_requirement_ids
    {
        return Err("repair delta review obligations are invalid".to_string());
    }

    if delta
        .path_changes
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
        || delta.path_changes.iter().any(|change| {
            canonical_repair_path(&change.path, false).as_deref() != Ok(change.path.as_str())
                || !baseline
                    .repair_scope
                    .paths
                    .iter()
                    .any(|scope| repair_scope_matches(scope, &change.path) == Ok(true))
                || change.before_exists != change.before_hash.is_some()
                || change.after_exists != change.after_hash.is_some()
                || match &change.change {
                    RepairPathChangeKind::Added => change.before_exists || !change.after_exists,
                    RepairPathChangeKind::Removed => !change.before_exists || change.after_exists,
                    RepairPathChangeKind::Modified => {
                        !change.before_exists
                            || !change.after_exists
                            || change.before_hash == change.after_hash
                    }
                }
        })
    {
        return Err("repair delta path changes are not canonical".to_string());
    }

    if delta
        .new_command_receipts
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
        || delta.new_command_receipts.iter().any(|receipt| {
            receipt.sequence <= baseline.command_sequence_high_water_mark
                || receipt.receipt_id.trim().is_empty()
        })
        || delta
            .new_command_receipts
            .iter()
            .map(|receipt| receipt.receipt_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != delta.new_command_receipts.len()
    {
        return Err("repair delta command receipts are not canonical".to_string());
    }

    if delta
        .invalidated_command_receipts
        .windows(2)
        .any(|pair| pair[0].sequence >= pair[1].sequence)
        || delta.invalidated_command_receipts.iter().any(|receipt| {
            let Some(binding) = baseline
                .command_bindings
                .iter()
                .find(|binding| binding.sequence == receipt.sequence)
            else {
                return true;
            };
            binding.receipt_id != receipt.receipt_id
                || receipt.reason != "implementation_identity_binding_mismatch"
                || binding.implementation_identity.as_deref()
                    == Some(delta.candidate_implementation_identity.as_str())
        })
    {
        return Err("repair delta command invalidations are invalid".to_string());
    }

    if !is_strictly_sorted_unique(&delta.newly_realized_surfaces)
        || delta.newly_realized_surfaces.iter().any(|surface| {
            !structured_surface_is_canonical(surface)
                || baseline.implementation_surfaces.contains(surface)
                || !baseline.repair_scope.surfaces.contains(surface)
        })
    {
        return Err("repair delta contract surfaces are invalid".to_string());
    }
    Ok(())
}

fn validate_rereview_audit_metadata(
    receipt: &CompletionReviewReceiptV2,
    parent: Option<&CompletionReviewReceiptV2>,
    manifest: Option<&RequirementManifestSnapshot>,
) -> Result<(), String> {
    let has_audit_metadata = receipt.repair_baseline.is_some()
        || receipt.baseline_hash.is_some()
        || receipt.input_mode.is_some()
        || receipt.delta_hash.is_some()
        || receipt.rereview_delta.is_some()
        || !receipt.fallback_reasons.is_empty()
        || receipt.candidate_implementation_identity.is_some()
        || receipt.rereview_audit_hash.is_some();

    match receipt.attempt_kind {
        CompletionReviewAttemptKind::InitialReview => {
            if receipt.input_mode.is_some()
                || receipt.delta_hash.is_some()
                || receipt.rereview_delta.is_some()
                || !receipt.fallback_reasons.is_empty()
                || receipt.candidate_implementation_identity.is_some()
                || receipt.rereview_audit_hash.is_some()
            {
                return Err("initial review contains rereview audit metadata".to_string());
            }
            match (
                receipt.repair_instruction_hash.as_ref(),
                receipt.repair_baseline.as_ref(),
                receipt.baseline_hash.as_ref(),
            ) {
                (None, None, None) | (Some(_), None, None) => Ok(()),
                (Some(_), Some(baseline), Some(baseline_hash)) => {
                    let manifest = manifest.ok_or_else(|| {
                        "repair baseline has no exact requirement manifest".to_string()
                    })?;
                    validate_persisted_repair_baseline(baseline, receipt, manifest)?;
                    if repair_baseline_hash(baseline) != *baseline_hash {
                        return Err("repair baseline hash is invalid".to_string());
                    }
                    Ok(())
                }
                _ => Err("initial review repair baseline metadata is incomplete".to_string()),
            }
        }
        CompletionReviewAttemptKind::CorrectionEvidence
        | CompletionReviewAttemptKind::TerminalClosure => {
            if has_audit_metadata {
                Err("non-review receipt contains repair-delta audit metadata".to_string())
            } else {
                Ok(())
            }
        }
        CompletionReviewAttemptKind::Rereview => {
            if !has_audit_metadata {
                return Ok(());
            }
            if receipt.repair_baseline.is_some() {
                return Err("rereview duplicates the initial repair baseline".to_string());
            }
            let parent = parent
                .filter(|parent| parent.attempt_kind == CompletionReviewAttemptKind::InitialReview)
                .ok_or_else(|| "rereview audit metadata lacks its initial parent".to_string())?;
            let input_mode = receipt
                .input_mode
                .ok_or_else(|| "rereview input mode is missing".to_string())?;
            let candidate_identity = receipt
                .candidate_implementation_identity
                .as_ref()
                .ok_or_else(|| "rereview candidate identity is missing".to_string())?;
            let repair_instruction_hash = receipt
                .repair_instruction_hash
                .as_ref()
                .ok_or_else(|| "rereview repair instruction hash is missing".to_string())?;
            if candidate_identity != &receipt.implementation_identity_hash
                || parent.repair_instruction_hash.as_ref() != Some(repair_instruction_hash)
                || !is_strictly_sorted_unique(&receipt.fallback_reasons)
            {
                return Err("rereview audit lineage is invalid".to_string());
            }

            match input_mode {
                RereviewInputMode::Delta => {
                    let baseline = parent.repair_baseline.as_ref().ok_or_else(|| {
                        "delta rereview parent has no repair baseline".to_string()
                    })?;
                    let parent_baseline_hash = parent
                        .baseline_hash
                        .as_ref()
                        .ok_or_else(|| "delta rereview parent has no baseline hash".to_string())?;
                    if receipt.baseline_hash.as_ref() != Some(parent_baseline_hash)
                        || repair_baseline_hash(baseline) != *parent_baseline_hash
                        || !receipt.fallback_reasons.is_empty()
                    {
                        return Err("delta rereview baseline binding is invalid".to_string());
                    }
                    let delta = receipt
                        .rereview_delta
                        .as_ref()
                        .ok_or_else(|| "delta rereview payload is missing".to_string())?;
                    let delta_hash = receipt
                        .delta_hash
                        .as_ref()
                        .ok_or_else(|| "delta rereview hash is missing".to_string())?;
                    if repair_delta_hash(delta) != *delta_hash
                        || delta.baseline_hash != *parent_baseline_hash
                        || delta.repair_instruction_hash != *repair_instruction_hash
                        || delta.candidate_implementation_identity != *candidate_identity
                    {
                        return Err("delta rereview binding is invalid".to_string());
                    }
                    validate_repair_delta_contents(delta, baseline, &parent.findings)?;
                }
                RereviewInputMode::FullFallback => {
                    if receipt.delta_hash.is_some()
                        || receipt.rereview_delta.is_some()
                        || receipt.fallback_reasons.is_empty()
                        || receipt.baseline_hash != parent.baseline_hash
                    {
                        return Err("full-fallback rereview metadata is invalid".to_string());
                    }
                }
            }

            let input = RereviewInput {
                input_mode,
                baseline_hash: receipt.baseline_hash.clone(),
                delta_hash: receipt.delta_hash.clone(),
                fallback_reasons: receipt.fallback_reasons.clone(),
                repair_instruction_hash: repair_instruction_hash.clone(),
                candidate_implementation_identity: candidate_identity.clone(),
                delta: receipt.rereview_delta.clone(),
            };
            let audit_hash = rereview_audit_hash(&input);
            if receipt.rereview_audit_hash.as_ref() != Some(&audit_hash) {
                return Err("rereview audit hash is invalid".to_string());
            }
            Ok(())
        }
    }
}

fn validate_v5_review_receipts(
    document: &TaskEvidenceDocument,
    ledger: &CompletionReviewLedgerV2,
) -> Result<(), String> {
    let manifest_by_key = ledger
        .manifest_snapshots
        .iter()
        .map(|manifest| {
            (
                (manifest.completion_epoch, manifest.manifest_revision),
                manifest,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeMap::<String, &CompletionReviewReceiptV2>::new();
    let mut max_sequence = 0;
    for (receipt_index, receipt) in ledger.receipts.iter().enumerate() {
        if receipt.review_id.trim().is_empty() || seen.contains_key(&receipt.review_id) {
            return Err("review receipt ID is blank or duplicated".to_string());
        }
        let migrated_v4 = receipt.infrastructure_outcome == "migrated_v4_terminal";
        let coordinates = parse_review_coordinates(&receipt.review_id);
        let manifest = if migrated_v4 {
            None
        } else {
            let (epoch, revision, sequence) =
                coordinates.ok_or_else(|| "review receipt ID is not host-canonical".to_string())?;
            max_sequence = max_sequence.max(sequence);
            let manifest = manifest_by_key
                .get(&(epoch, revision))
                .copied()
                .ok_or_else(|| "review receipt has no exact manifest snapshot".to_string())?;
            if receipt.requirement_manifest_hash != manifest.manifest_hash {
                return Err("review receipt is bound to the wrong manifest hash".to_string());
            }
            let source_hash_matches = [false, true].into_iter().any(|source_capture_failed| {
                receipt.user_source_ledger_hash
                    == user_source_ledger_snapshot_hash(
                        ledger,
                        epoch,
                        revision,
                        source_capture_failed,
                    )
            });
            if !source_hash_matches {
                return Err("review receipt is bound to the wrong user-source snapshot".to_string());
            }
            Some(manifest)
        };
        if receipt.candidate_hash != receipt.implementation_identity_hash
            || receipt.implementation_identity_hash.trim().is_empty()
            || receipt.dossier_snapshot_id.trim().is_empty()
            || receipt.infrastructure_outcome.trim().is_empty()
        {
            return Err("review receipt identity or infrastructure outcome is invalid".to_string());
        }
        let parent =
            match receipt.parent_review_id.as_ref() {
                Some(parent_id) => Some(*seen.get(parent_id).ok_or_else(|| {
                    "review receipt parent is missing or not earlier".to_string()
                })?),
                None => None,
            };
        let superseded = match receipt.superseded_review_id.as_ref() {
            Some(review_id) => Some(
                *seen
                    .get(review_id)
                    .ok_or_else(|| "superseded review is missing or not earlier".to_string())?,
            ),
            None => None,
        };
        let active_requirement_ids = manifest
            .map(|manifest| {
                manifest
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.status == RequirementStatus::Active)
                    .map(|requirement| requirement.requirement_id.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let valid_requirement_ids = manifest
            .map(|manifest| {
                manifest
                    .requirements
                    .iter()
                    .map(|requirement| requirement.requirement_id.as_str())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        for (index, finding) in receipt.findings.iter().enumerate() {
            if finding.finding_id != format!("{}/F{}", receipt.review_id, index + 1)
                || finding
                    .requirement_ids
                    .iter()
                    .any(|requirement_id| !valid_requirement_ids.contains(requirement_id.as_str()))
                || (!finding.requirement_ids.is_empty()
                    && !finding.requirement_ids.iter().any(|requirement_id| {
                        active_requirement_ids.contains(requirement_id.as_str())
                    }))
                || !COMPLETION_REVIEW_LENSES.contains(&finding.lens.as_str())
                || finding.contract_surface.trim().is_empty()
                || finding.severity.trim().is_empty()
                || finding.evidence.trim().is_empty()
                || finding.smallest_correction.trim().is_empty()
                || finding.proof_route.trim().is_empty()
            {
                return Err("review finding is not canonical or receipt-local".to_string());
            }
        }
        if receipt.review_clean && !receipt.findings.is_empty() {
            return Err("clean review receipt contains findings".to_string());
        }
        if receipt.manifest_gaps.iter().any(|gap| {
            let Some(source) = ledger.source_records.get(&gap.source_id) else {
                return true;
            };
            gap.omitted_spans.is_empty()
                || gap.omitted_spans.iter().any(|span| {
                    let Some(material) = material_for_span(source, span) else {
                        return true;
                    };
                    material.trim().is_empty()
                        || manifest.is_some_and(|manifest| {
                            let requirement_id = deterministic_requirement_id(source, span);
                            manifest
                                .requirements
                                .iter()
                                .any(|requirement| requirement.requirement_id == requirement_id)
                        })
                })
        }) {
            return Err(
                "manifest gap does not identify omitted immutable source material".to_string(),
            );
        }
        validate_rereview_audit_metadata(receipt, parent, manifest)?;
        match receipt.attempt_kind {
            CompletionReviewAttemptKind::InitialReview => {
                if !receipt.dispositions.is_empty()
                    || parent.is_some_and(|parent| {
                        parent.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
                    })
                    || superseded.is_some_and(|superseded| {
                        !manifest_gap_supersession_is_valid(receipt, superseded)
                    })
                {
                    return Err("initial review lineage is invalid".to_string());
                }
            }
            CompletionReviewAttemptKind::CorrectionEvidence => {
                let Some(parent) = parent else {
                    return Err("correction evidence lacks its initial-review parent".to_string());
                };
                if parent.attempt_kind != CompletionReviewAttemptKind::InitialReview
                    || parent.repair_instruction_hash.is_none()
                    || receipt.repair_instruction_hash != parent.repair_instruction_hash
                    || !receipt.findings.is_empty()
                    || !receipt.dispositions.is_empty()
                    || !receipt.manifest_gaps.is_empty()
                    || receipt.review_clean
                    || receipt.terminal_outcome.is_some()
                {
                    return Err("correction evidence lineage is invalid".to_string());
                }
            }
            CompletionReviewAttemptKind::Rereview => {
                let Some(parent) = parent else {
                    return Err("rereview lacks its initial-review parent".to_string());
                };
                if parent.attempt_kind != CompletionReviewAttemptKind::InitialReview {
                    return Err("rereview parent or manifest-gap state is invalid".to_string());
                }
                let correction = ledger.receipts[..receipt_index]
                    .iter()
                    .rev()
                    .find(|candidate| {
                        candidate.attempt_kind == CompletionReviewAttemptKind::CorrectionEvidence
                            && candidate.parent_review_id.as_deref()
                                == Some(parent.review_id.as_str())
                    })
                    .ok_or_else(|| "rereview lacks correction evidence".to_string())?;
                if correction.infrastructure_outcome != "ok"
                    || correction.terminal_outcome.is_some()
                    || correction.repair_instruction_hash != parent.repair_instruction_hash
                    || receipt.repair_instruction_hash != parent.repair_instruction_hash
                    || !receipt_identity_matches(receipt, correction)
                {
                    return Err("rereview is not bound to its correction evidence".to_string());
                }
                if receipt.infrastructure_outcome == "ok" {
                    let expected = parent
                        .findings
                        .iter()
                        .map(|finding| finding.finding_id.as_str())
                        .collect::<BTreeSet<_>>();
                    let returned = receipt
                        .dispositions
                        .iter()
                        .map(|disposition| disposition.finding_id.as_str())
                        .collect::<BTreeSet<_>>();
                    if returned.len() != receipt.dispositions.len()
                        || returned != expected
                        || receipt.dispositions.iter().any(|disposition| {
                            !matches!(
                                disposition.disposition.as_str(),
                                "resolved"
                                    | "rebuttal_accepted"
                                    | "still_present"
                                    | "insufficient_proof"
                                    | "regressed"
                            ) || disposition.evidence.trim().is_empty()
                        })
                    {
                        return Err(
                            "rereview dispositions do not exactly cover original findings"
                                .to_string(),
                        );
                    }
                } else if !receipt.findings.is_empty()
                    || !receipt.dispositions.is_empty()
                    || !receipt.manifest_gaps.is_empty()
                    || receipt.review_clean
                    || receipt.terminal_outcome.is_some()
                    || superseded.is_some()
                {
                    return Err(
                        "failed rereview receipt contains reviewer-authored or terminal state"
                            .to_string(),
                    );
                }
            }
            CompletionReviewAttemptKind::TerminalClosure => {
                let Some(parent) = parent else {
                    return Err("terminal closure lacks its accepted review parent".to_string());
                };
                let infrastructure_outcome_is_valid = receipt.infrastructure_outcome == "ok"
                    || (migrated_v4 && parent.infrastructure_outcome == "migrated_v4_terminal");
                if !matches!(
                    parent.attempt_kind,
                    CompletionReviewAttemptKind::InitialReview
                        | CompletionReviewAttemptKind::Rereview
                ) || parent.terminal_outcome.is_some()
                    || !receipt_identity_matches(receipt, parent)
                    || !receipt.findings.is_empty()
                    || !receipt.dispositions.is_empty()
                    || !receipt.manifest_gaps.is_empty()
                    || receipt.repair_instruction_hash.is_some()
                    || !infrastructure_outcome_is_valid
                {
                    return Err("terminal closure is not bound to its exact review".to_string());
                }
                match receipt.terminal_outcome.as_deref() {
                    Some("passed")
                        if receipt.review_clean && parent.review_clean && superseded.is_none() => {}
                    Some("partial") if !receipt.review_clean => {
                        if let Some(superseded) = superseded
                            && (superseded.attempt_kind
                                != CompletionReviewAttemptKind::TerminalClosure
                                || superseded.terminal_outcome.as_deref() != Some("passed")
                                || !receipt_identity_matches(receipt, superseded))
                        {
                            return Err(
                                "partial terminal closure supersession is invalid".to_string()
                            );
                        }
                    }
                    Some("blocked") if !receipt.review_clean && superseded.is_none() => {}
                    _ => return Err("terminal closure outcome is invalid".to_string()),
                }
            }
        }
        seen.insert(receipt.review_id.clone(), receipt);
    }
    if ledger.next_review_sequence <= max_sequence {
        return Err("next review sequence can reuse a persisted review identity".to_string());
    }
    if let Some(cycle) = ledger.active_review_cycle.as_ref() {
        if cycle.manifest_revision != ledger.manifest_revision {
            return Err("active review cycle targets the wrong manifest revision".to_string());
        }
        if let Some(parent_terminal) = cycle.parent_terminal_review_id.as_ref()
            && seen.get(parent_terminal).is_none_or(|receipt| {
                receipt.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
            })
        {
            return Err("active cycle parent terminal receipt is invalid".to_string());
        }
        if let Some(superseded_review_id) = cycle.superseded_review_id.as_ref() {
            let superseded = seen
                .get(superseded_review_id)
                .copied()
                .ok_or_else(|| "active cycle superseded review is missing".to_string())?;
            let Some((epoch, revision, _)) = parse_review_coordinates(&superseded.review_id) else {
                return Err("active cycle superseded review ID is invalid".to_string());
            };
            if !matches!(
                superseded.attempt_kind,
                CompletionReviewAttemptKind::InitialReview | CompletionReviewAttemptKind::Rereview
            ) || superseded.manifest_gaps.is_empty()
                || epoch != ledger.completion_epoch
                || revision >= cycle.manifest_revision
            {
                return Err("active cycle manifest-gap supersession is invalid".to_string());
            }
            if let Some(current_initial) = ledger.receipts.iter().rev().find(|receipt| {
                receipt.attempt_kind == CompletionReviewAttemptKind::InitialReview
                    && parse_review_coordinates(&receipt.review_id).is_some_and(
                        |(receipt_epoch, receipt_revision, _)| {
                            receipt_epoch == ledger.completion_epoch
                                && receipt_revision == cycle.manifest_revision
                        },
                    )
            }) && current_initial.superseded_review_id.as_deref()
                != Some(superseded_review_id.as_str())
            {
                return Err(
                    "replacement initial review lost its manifest-gap supersession".to_string(),
                );
            }
        }
        match cycle.accepted_review_id.as_ref() {
            Some(accepted_id) => {
                let accepted = seen
                    .get(accepted_id)
                    .copied()
                    .ok_or_else(|| "active cycle accepted review is missing".to_string())?;
                if !accepted.review_clean
                    || accepted.terminal_outcome.is_some()
                    || !matches!(
                        accepted.attempt_kind,
                        CompletionReviewAttemptKind::InitialReview
                            | CompletionReviewAttemptKind::Rereview
                    )
                    || cycle.accepted_dossier_snapshot_id.as_deref()
                        != Some(accepted.dossier_snapshot_id.as_str())
                {
                    return Err("active cycle accepted review pointer is inconsistent".to_string());
                }
            }
            None if cycle.accepted_dossier_snapshot_id.is_some() => {
                return Err(
                    "active cycle has a dossier pointer without an accepted review".to_string(),
                );
            }
            None => {}
        }
        if matches!(
            cycle.phase,
            CompletionReviewCyclePhase::ProvisionalClean | CompletionReviewCyclePhase::Closed
        ) && cycle.accepted_review_id.is_none()
        {
            return Err("clean or closed cycle lacks an accepted review".to_string());
        }
    }
    let completion_passed = document
        .completion
        .as_ref()
        .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
    if completion_passed {
        let cycle = ledger
            .active_review_cycle
            .as_ref()
            .ok_or_else(|| "Passed completion lacks an active cycle".to_string())?;
        let accepted_id = cycle
            .accepted_review_id
            .as_ref()
            .ok_or_else(|| "Passed completion lacks an accepted review".to_string())?;
        let terminal_id = ledger
            .last_terminal_closure
            .as_ref()
            .ok_or_else(|| "Passed completion lacks terminal closure".to_string())?;
        let terminal = seen
            .get(terminal_id)
            .copied()
            .ok_or_else(|| "Passed terminal closure is missing".to_string())?;
        if cycle.phase != CompletionReviewCyclePhase::Closed
            || ledger.review_risk.unresolved
            || ledger.review_risk.resolved_at.is_none()
            || terminal.attempt_kind != CompletionReviewAttemptKind::TerminalClosure
            || terminal.terminal_outcome.as_deref() != Some("passed")
            || terminal.parent_review_id.as_ref() != Some(accepted_id)
            || seen
                .get(accepted_id)
                .is_none_or(|accepted| !receipt_identity_matches(terminal, accepted))
        {
            return Err("Passed completion lacks an exact atomic terminal closure".to_string());
        }
    }
    if !ledger.review_risk.unresolved
        && ledger.review_risk.resolved_at.is_some()
        && ledger.last_terminal_closure.is_none()
    {
        return Err("resolved completion-review risk lacks terminal closure".to_string());
    }
    Ok(())
}

fn uses_retired_v3_completion_shape(schema_version: u32, value: &Value) -> bool {
    schema_version == 3
        && [
            "validation_epoch",
            "next_validation_receipt_sequence",
            "validation_receipts",
            "wiring_receipt",
        ]
        .iter()
        .any(|field| value.get(*field).is_some())
}

fn recorded_repository_root_matches(recorded: &str, expected: &Path) -> bool {
    let recorded = Path::new(recorded);
    recorded.is_absolute() && repository_roots_match(recorded, expected)
}

fn canonical_repository_root(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn repository_roots_match(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(left), Ok(right)) => repository_root_paths_equal(&left, &right),
        _ => repository_root_paths_equal(left, right),
    }
}

#[cfg(not(windows))]
fn repository_root_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(windows)]
fn repository_root_paths_equal(left: &Path, right: &Path) -> bool {
    use std::path::Component;
    use std::path::Prefix;

    let mut left_components = left.components();
    let mut right_components = right.components();
    let left_drive = match left_components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => Some(drive),
            _ => None,
        },
        _ => None,
    };
    let right_drive = match right_components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => Some(drive),
            _ => None,
        },
        _ => None,
    };

    match (left_drive, right_drive) {
        (Some(left_drive), Some(right_drive)) => {
            left_drive.eq_ignore_ascii_case(&right_drive) && left_components.eq(right_components)
        }
        _ => left == right,
    }
}

async fn quarantine_evidence_file(path: &Path, kind: &str) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("task-evidence.json");
    let quarantine = path.with_file_name(format!(
        "{file_name}.{kind}.{}.preserved",
        uuid::Uuid::now_v7()
    ));
    tokio::fs::rename(path, &quarantine).await?;
    Ok(quarantine)
}

#[cfg(test)]
fn migrate_document(document: &mut TaskEvidenceDocument) {
    let legacy_completion_model = document.schema_version < TASK_EVIDENCE_COMPLETION_MODEL_VERSION;
    migrate_document_with_completion_model(document, legacy_completion_model);
}

fn migrate_document_with_completion_model(
    document: &mut TaskEvidenceDocument,
    legacy_completion_model: bool,
) {
    let source_schema_version = document.schema_version;
    document.next_edit_receipt_sequence =
        document
            .next_edit_receipt_sequence
            .max(next_sequence_after_ids(
                document
                    .edit_receipts
                    .iter()
                    .map(|receipt| receipt.id.as_str()),
            ));
    document.next_command_receipt_sequence =
        document
            .next_command_receipt_sequence
            .max(next_sequence_after_ids(
                document
                    .command_receipts
                    .iter()
                    .map(|receipt| receipt.id.as_str()),
            ));
    document.next_external_evidence_receipt_sequence = document
        .next_external_evidence_receipt_sequence
        .max(next_sequence_after_ids(
            document
                .external_evidence
                .iter()
                .map(|receipt| receipt.id.as_str()),
        ));
    let (duplicate_edit_indices, _) = duplicate_receipt_indices(
        document
            .edit_receipts
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_edit_indices {
        let id = next_receipt_id("edit", &mut document.next_edit_receipt_sequence);
        document.edit_receipts[index].id = id;
    }
    let (duplicate_command_indices, _) = duplicate_receipt_indices(
        document
            .command_receipts
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_command_indices {
        let id = next_receipt_id("command", &mut document.next_command_receipt_sequence);
        document.command_receipts[index].id = id;
    }
    let (duplicate_external_indices, _) = duplicate_receipt_indices(
        document
            .external_evidence
            .iter()
            .enumerate()
            .map(|(index, receipt)| (index, receipt.id.as_str())),
    );
    for index in duplicate_external_indices {
        let id = next_receipt_id(
            "external-evidence",
            &mut document.next_external_evidence_receipt_sequence,
        );
        document.external_evidence[index].id = id;
    }
    let owned_file_paths = task_owned_file_paths(document);
    document
        .latest_file_hashes
        .retain(|path, _| owned_file_paths.contains(path));
    let mut used_ids = BTreeSet::new();
    let mut duplicate_step_ids = BTreeSet::new();
    for (index, step) in document.plan.iter_mut().enumerate() {
        if !used_ids.insert(step.id.clone()) {
            duplicate_step_ids.insert(step.id.clone());
            step.id = unique_step_id(&step.id, index, &mut used_ids);
            if step.status == StepStatus::Passed {
                step.status = StepStatus::Implemented;
            }
        }
    }
    if legacy_completion_model {
        for step in &mut document.plan {
            if matches!(step.status, StepStatus::Passed | StepStatus::Completed) {
                step.status = StepStatus::Implemented;
            }
        }
        document.completion = None;
        document.risks.retain(|risk| {
            matches!(
                risk.source.as_str(),
                "edit"
                    | "command"
                    | "freshness"
                    | "plan"
                    | "plan_structure"
                    | "task_evidence_storage"
            )
        });
        document.latest_generated_artifact_hashes.clear();
    }
    rebuild_declared_requirements_and_risks(document);
    sync_plan_structure_state(document, &duplicate_step_ids);
    if document.completion_review_v2.is_none() {
        document.completion_review_v2 = Some(new_completion_review_ledger(&document.thread_id));
    }
    if source_schema_version == FROZEN_TASK_EVIDENCE_V4_SCHEMA_VERSION
        && document
            .completion
            .as_ref()
            .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
    {
        seed_migrated_v4_terminal_lineage(document);
    }
    if source_schema_version <= FROZEN_TASK_EVIDENCE_V5_SCHEMA_VERSION {
        document.source_classification_cache.clear();
    } else {
        document.source_classification_cache = canonical_source_classification_cache(
            std::mem::take(&mut document.source_classification_cache),
        );
    }
    document.schema_version = TASK_EVIDENCE_SCHEMA_VERSION;
}

fn seed_migrated_v4_terminal_lineage(document: &mut TaskEvidenceDocument) {
    let Some(mut ledger) = document.completion_review_v2.take() else {
        return;
    };
    if !ledger.receipts.is_empty() || ledger.active_review_cycle.is_some() {
        document.completion_review_v2 = Some(ledger);
        return;
    }

    let recorded_at = document.updated_at.clone();
    let source_ledger_hash = canonical_hash(
        USER_SOURCE_LEDGER_CANONICAL_FORMAT,
        &serde_json::json!({
            "rootTaskId": ledger.root_task_id,
            "completionEpoch": ledger.completion_epoch,
            "manifestRevision": ledger.manifest_revision,
            "sources": [],
            "mappings": {},
            "sourceCaptureFailed": false,
        }),
    );
    let manifest_hash = requirement_manifest_hash(ledger.manifest_revision, &[]);
    let implementation_identity_hash = canonical_hash(
        IMPLEMENTATION_IDENTITY_CANONICAL_FORMAT,
        &serde_json::json!({
            "migration": "v4_terminal_passed",
            "rootTaskId": ledger.root_task_id,
            "completionEpoch": ledger.completion_epoch,
            "manifestRevision": ledger.manifest_revision,
            "userSourceLedgerHash": source_ledger_hash,
            "requirementManifestHash": manifest_hash,
            "mutationRevision": document.host_mutation_revision,
        }),
    );
    let dossier_snapshot_id = canonical_hash(
        DOSSIER_SNAPSHOT_CANONICAL_FORMAT,
        &serde_json::json!({
            "migration": "v4_terminal_passed",
            "implementationIdentity": implementation_identity_hash,
            "legacyCompletion": document.completion,
        }),
    );
    let initial_review_id = "review-1-0-migrated-v4-initial".to_string();
    let terminal_review_id = "review-1-0-migrated-v4-terminal".to_string();
    let receipt = |review_id: String,
                   attempt_kind: CompletionReviewAttemptKind,
                   parent_review_id: Option<String>,
                   terminal_outcome: Option<String>| CompletionReviewReceiptV2 {
        review_id,
        attempt_kind,
        parent_review_id,
        superseded_review_id: None,
        candidate_mutation_revision: document.host_mutation_revision,
        candidate_hash: implementation_identity_hash.clone(),
        implementation_identity_hash: implementation_identity_hash.clone(),
        dossier_snapshot_id: dossier_snapshot_id.clone(),
        user_source_ledger_hash: source_ledger_hash.clone(),
        requirement_manifest_hash: manifest_hash.clone(),
        findings: Vec::new(),
        dispositions: Vec::new(),
        manifest_gaps: Vec::new(),
        repair_instruction_hash: None,
        repair_baseline: None,
        baseline_hash: None,
        input_mode: None,
        delta_hash: None,
        rereview_delta: None,
        fallback_reasons: Vec::new(),
        candidate_implementation_identity: None,
        rereview_audit_hash: None,
        infrastructure_outcome: "migrated_v4_terminal".to_string(),
        review_clean: true,
        terminal_outcome,
        recorded_at: recorded_at.clone(),
    };
    ledger.receipts.push(receipt(
        initial_review_id.clone(),
        CompletionReviewAttemptKind::InitialReview,
        None,
        None,
    ));
    ledger.receipts.push(receipt(
        terminal_review_id.clone(),
        CompletionReviewAttemptKind::TerminalClosure,
        Some(initial_review_id.clone()),
        Some("passed".to_string()),
    ));
    ledger.active_review_cycle = Some(CompletionReviewCycle {
        cycle_id: "cycle-1-0-migrated-v4".to_string(),
        manifest_revision: ledger.manifest_revision,
        parent_terminal_review_id: None,
        superseded_review_id: None,
        phase: CompletionReviewCyclePhase::Closed,
        correction_consumed: false,
        manifest_gap_reconstructed: false,
        accepted_review_id: Some(initial_review_id),
        accepted_dossier_snapshot_id: Some(dossier_snapshot_id),
    });
    ledger.review_risk = CompletionReviewRisk {
        unresolved: false,
        cycle_id: Some("cycle-1-0-migrated-v4".to_string()),
        opened_at: Some(recorded_at.clone()),
        resolved_at: Some(recorded_at),
    };
    ledger.last_terminal_closure = Some(terminal_review_id);
    ledger.next_review_sequence = 3;
    document.completion_review_v2 = Some(ledger);
}

const fn initial_receipt_sequence() -> u64 {
    1
}

fn next_sequence_after_ids<'a>(ids: impl Iterator<Item = &'a str>) -> u64 {
    ids.filter_map(|id| id.rsplit_once('-')?.1.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(initial_receipt_sequence())
}

fn next_receipt_id(prefix: &str, sequence: &mut u64) -> String {
    let current = (*sequence).max(initial_receipt_sequence());
    *sequence = current.saturating_add(1);
    format!("{prefix}-{current}")
}

fn duplicate_receipt_indices<'a>(
    ids: impl Iterator<Item = (usize, &'a str)>,
) -> (Vec<usize>, BTreeSet<String>) {
    let mut seen = BTreeSet::new();
    let mut duplicate_indices = Vec::new();
    let mut duplicate_ids = BTreeSet::new();
    for (index, id) in ids {
        if !seen.insert(id.to_string()) {
            duplicate_indices.push(index);
            duplicate_ids.insert(id.to_string());
        }
    }
    (duplicate_indices, duplicate_ids)
}

fn atomic_write_evidence(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("task-evidence path {} has no parent", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file_mut().sync_all()?;
    let persisted = temp.persist(path).map_err(|err| err.error)?;
    persisted.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn find_kd4_repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|candidate| candidate.join("kd4_features.toml").is_file())
        .map(Path::to_path_buf)
}

fn task_owned_file_paths(document: &TaskEvidenceDocument) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for step in &document.plan {
        paths.extend(step.edit_paths.iter().cloned());
    }
    for intent in &document.edit_intents {
        paths.extend(intent.files.iter().map(|file| file.path.clone()));
    }
    for receipt in &document.edit_receipts {
        paths.extend(receipt.files.iter().map(|file| file.path.clone()));
    }
    paths
}

fn effective_step_id(item: &PlanItemArg, index: usize, used_ids: &mut BTreeSet<String>) -> String {
    if let Some(id) = item.id.as_ref() {
        if used_ids.insert(id.clone()) {
            return id.clone();
        }
        return unique_step_id(id, index, used_ids);
    }
    let digest = sha1_hex(item.step.trim().as_bytes());
    let base = format!("step-{}", &digest[..12]);
    if used_ids.insert(base.clone()) {
        return base;
    }
    unique_step_id(&base, index, used_ids)
}

fn unique_step_id(base: &str, index: usize, used_ids: &mut BTreeSet<String>) -> String {
    let mut suffix = index.saturating_add(1);
    loop {
        let candidate = format!("{base}-{suffix}");
        if used_ids.insert(candidate.clone()) {
            return candidate;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn normalize_requested_status(requested: &StepStatus) -> StepStatus {
    match requested {
        StepStatus::Completed => StepStatus::Passed,
        status => status.clone(),
    }
}

fn step_materially_matches_item(step: &EvidencePlanStep, item: &PlanItemArg) -> bool {
    step.step == item.step
        && step.depends_on == item.depends_on
        && step.acceptance_criteria == item.acceptance_criteria
        && step.runtime_paths == item.runtime_paths
        && step.generated_artifacts == item.generated_artifacts
        && step.risks == item.risks
        && step.requires_desktop_activation == item.requires_desktop_activation
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

fn sync_plan_structure_state(
    document: &mut TaskEvidenceDocument,
    duplicate_explicit_ids: &BTreeSet<String>,
) {
    let active_ids = document
        .plan
        .iter()
        .filter(|step| step.status == StepStatus::InProgress)
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();
    document.active_step_id = if active_ids.len() == 1 {
        active_ids.first().cloned()
    } else {
        None
    };
    if active_ids.len() > 1 {
        upsert_risk(
            document,
            EvidenceRisk {
                id: "plan-structure-multiple-active-steps".to_string(),
                description: format!(
                    "plan declares multiple in-progress steps: {}",
                    active_ids.join(", ")
                ),
                source: "plan_structure".to_string(),
                blocking: true,
                resolved: false,
                epoch: document.evidence_epoch,
            },
        );
    } else {
        resolve_risk(document, "plan-structure-multiple-active-steps");
    }
    if duplicate_explicit_ids.is_empty() {
        resolve_risk(document, "plan-structure-duplicate-step-ids");
    } else {
        upsert_risk(
            document,
            EvidenceRisk {
                id: "plan-structure-duplicate-step-ids".to_string(),
                description: format!(
                    "plan contained duplicate explicit step ids: {}",
                    duplicate_explicit_ids
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                source: "plan_structure".to_string(),
                blocking: true,
                resolved: false,
                epoch: document.evidence_epoch,
            },
        );
    }
}

fn rebuild_declared_requirements_and_risks(document: &mut TaskEvidenceDocument) {
    document.generated_artifact_requirements.clear();
    document.risks.retain(|risk| risk.source != "plan");
    let mut requirements = Vec::new();
    let mut risks = Vec::new();
    for step in &document.plan {
        if step.status != StepStatus::Skipped {
            for (index, path) in step.generated_artifacts.iter().enumerate() {
                requirements.push(GeneratedArtifactRequirement {
                    id: format!("plan:{}:artifact:{index}", step.id),
                    step_id: Some(step.id.clone()),
                    path: Some(normalize_slashes(path)),
                });
            }
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

fn plan_is_terminally_acknowledged(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        && document
            .plan
            .iter()
            .all(|step| matches!(step.status, StepStatus::Passed | StepStatus::Skipped))
}

fn resolve_recoverable_runtime_risks(document: &mut TaskEvidenceDocument) {
    for risk in &mut document.risks {
        if !risk.blocking && matches!(risk.source.as_str(), "edit" | "command" | "freshness") {
            risk.resolved = true;
        }
    }
}

fn edit_outcome_succeeded(outcome: &str) -> bool {
    outcome == "completed"
}

fn generated_artifact_is_currently_available(document: &TaskEvidenceDocument, path: &str) -> bool {
    let normalized = normalize_slashes(path);
    let Some(current) = document.latest_generated_artifact_hashes.get(&normalized) else {
        return false;
    };
    current.exists && current.read_error.is_none() && current.sha1.is_some()
}

fn dependency_cycle_members(document: &TaskEvidenceDocument) -> BTreeSet<String> {
    fn visit<'a>(
        id: &'a str,
        steps: &BTreeMap<&'a str, &'a EvidencePlanStep>,
        states: &mut BTreeMap<&'a str, u8>,
        stack: &mut Vec<&'a str>,
        cycle_members: &mut BTreeSet<String>,
    ) {
        match states.get(id).copied() {
            Some(2) => return,
            Some(1) => {
                if let Some(cycle_start) = stack.iter().position(|candidate| *candidate == id) {
                    cycle_members.extend(
                        stack[cycle_start..]
                            .iter()
                            .map(|member| (*member).to_string()),
                    );
                }
                return;
            }
            _ => {}
        }
        let Some(step) = steps.get(id) else {
            return;
        };
        states.insert(id, 1);
        stack.push(id);
        for dependency in &step.depends_on {
            if steps.contains_key(dependency.as_str()) {
                visit(dependency, steps, states, stack, cycle_members);
            }
        }
        stack.pop();
        states.insert(id, 2);
    }

    let steps = document
        .plan
        .iter()
        .filter(|step| step.status != StepStatus::Skipped)
        .map(|step| (step.id.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    let mut cycle_members = BTreeSet::new();
    for id in steps.keys().copied() {
        visit(id, &steps, &mut states, &mut stack, &mut cycle_members);
    }
    cycle_members
}

fn completion_review_locally_obtainable_proof_routes(gate: &TaskCompletionGate) -> Vec<String> {
    if gate.status != TaskCompletionStatus::Partial {
        return Vec::new();
    }
    let mut routes = gate
        .reasons
        .iter()
        .filter_map(|reason| {
            if reason == "no durable plan steps were recorded"
                || reason.starts_with("plan steps are not acknowledged as passed:")
                || (reason.starts_with("plan step `") && reason.contains("unfinished step"))
            {
                Some(format!(
                    "Complete the named durable plan obligation and attach its deterministic focused proof: {reason}"
                ))
            } else if reason == "required Desktop activation receipt is missing or stale" {
                Some(
                    "Desktop activation proof is unavailable until the native host implements a publish-ID-bound initialization handshake; command output cannot satisfy this requirement"
                        .to_string(),
                )
            } else if reason.starts_with(
                "required generated artifact is missing, unreadable, or unhashable:",
            ) {
                Some(format!(
                    "Regenerate or restore the named artifact through its owning generator, then record the focused proof: {reason}"
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    routes.sort();
    routes.dedup();
    routes
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
    let unacknowledged_steps = document
        .plan
        .iter()
        .filter(|step| !matches!(step.status, StepStatus::Passed | StepStatus::Skipped))
        .map(|step| format!("{} ({:?})", step.id, step.status))
        .collect::<Vec<_>>();
    if !unacknowledged_steps.is_empty() {
        partial.push(format!(
            "plan steps are not acknowledged as passed: {}",
            unacknowledged_steps.join(", ")
        ));
    }
    let steps_by_id = document
        .plan
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect::<BTreeMap<_, _>>();
    for step in document
        .plan
        .iter()
        .filter(|step| step.status != StepStatus::Skipped)
    {
        for dependency in &step.depends_on {
            if dependency == &step.id {
                blocked.push(format!("plan step `{}` cannot depend on itself", step.id));
                continue;
            }
            let Some(dependency_step) = steps_by_id.get(dependency.as_str()) else {
                blocked.push(format!(
                    "plan step `{}` depends on missing step `{dependency}`",
                    step.id
                ));
                continue;
            };
            if !matches!(
                dependency_step.status,
                StepStatus::Passed | StepStatus::Skipped
            ) {
                partial.push(format!(
                    "plan step `{}` depends on unfinished step `{dependency}` ({:?})",
                    step.id, dependency_step.status
                ));
            }
        }
    }
    let cycle_members = dependency_cycle_members(document);
    if !cycle_members.is_empty() {
        blocked.push(format!(
            "plan dependency cycle includes: {}",
            cycle_members.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if document
        .plan
        .iter()
        .any(|step| step.status != StepStatus::Skipped && step.requires_desktop_activation)
        && document
            .desktop_activation_receipt
            .as_ref()
            .is_none_or(|receipt| {
                receipt.epoch != document.evidence_epoch
                    || !desktop_activation_receipt_is_complete(receipt)
            })
    {
        partial.push("required Desktop activation receipt is missing or stale".to_string());
    }
    for requirement in &document.generated_artifact_requirements {
        if let Some(path) = requirement.path.as_ref()
            && !generated_artifact_is_currently_available(document, path)
        {
            partial.push(format!(
                "required generated artifact is missing, unreadable, or unhashable: {path}"
            ));
        }
    }
    for snapshot in document
        .latest_file_hashes
        .values()
        .filter(|snapshot| snapshot.read_error.is_some())
    {
        partial.push(format!(
            "task-controlled file is unreadable and cannot be freshness-checked: {}",
            snapshot.path
        ));
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

fn overlay_completion_review_gate(
    document: &TaskEvidenceDocument,
    gate: &mut TaskCompletionGate,
    source_capture_failed: bool,
) {
    let Some(ledger) = document.completion_review_v2.as_ref() else {
        return;
    };
    let cycle_phase = ledger.active_review_cycle.as_ref().map(|cycle| cycle.phase);
    let blocked = cycle_phase == Some(CompletionReviewCyclePhase::TerminalBlocked);
    let unresolved = source_capture_failed
        || ledger.review_risk.unresolved
        || cycle_phase.is_some_and(|phase| phase != CompletionReviewCyclePhase::Closed);
    if !unresolved && !blocked {
        return;
    }
    if source_capture_failed {
        gate.reasons.push(
            "a user source could not be durably captured before completion review".to_string(),
        );
    }
    if blocked {
        gate.reasons
            .push("completion review is blocked by an external impediment".to_string());
        gate.status = TaskCompletionStatus::Blocked;
    } else if gate.status != TaskCompletionStatus::Blocked {
        gate.reasons.push(
            "completion review risk remains unresolved for the current candidate".to_string(),
        );
        gate.status = TaskCompletionStatus::Partial;
    }
    gate.reasons.sort();
    gate.reasons.dedup();
}

fn invalidate_for_mutation(document: &mut TaskEvidenceDocument) {
    document.host_mutation_revision = document.host_mutation_revision.saturating_add(1);
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.last_mutation_at = Some(timestamp());
    document.desktop_activation_receipt = None;
    document.completion = None;
    if let Some(ledger) = document.completion_review_v2.as_mut()
        && let Some(cycle) = ledger.active_review_cycle.as_mut()
    {
        match cycle.phase {
            CompletionReviewCyclePhase::RereviewPending if cycle.correction_consumed => {
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
            }
            CompletionReviewCyclePhase::ProvisionalClean => {
                cycle.phase = CompletionReviewCyclePhase::InitialReviewPending;
                cycle.accepted_review_id = None;
                cycle.accepted_dossier_snapshot_id = None;
                ledger.review_risk.unresolved = true;
                ledger.review_risk.resolved_at = None;
            }
            CompletionReviewCyclePhase::TerminalPartial
            | CompletionReviewCyclePhase::TerminalBlocked => {
                let parent_terminal_review_id = ledger
                    .receipts
                    .iter()
                    .rev()
                    .find(|receipt| {
                        receipt.attempt_kind == CompletionReviewAttemptKind::TerminalClosure
                    })
                    .map(|receipt| receipt.review_id.clone());
                *cycle = CompletionReviewCycle {
                    cycle_id: format!(
                        "cycle-{}-{}-resume-{}",
                        ledger.completion_epoch,
                        ledger.manifest_revision,
                        document.host_mutation_revision
                    ),
                    manifest_revision: ledger.manifest_revision,
                    parent_terminal_review_id,
                    superseded_review_id: None,
                    phase: CompletionReviewCyclePhase::InitialReviewPending,
                    correction_consumed: false,
                    manifest_gap_reconstructed: false,
                    accepted_review_id: None,
                    accepted_dossier_snapshot_id: None,
                };
                ledger.review_risk.unresolved = true;
                ledger.review_risk.cycle_id = Some(cycle.cycle_id.clone());
                ledger.review_risk.opened_at = Some(timestamp());
                ledger.review_risk.resolved_at = None;
            }
            CompletionReviewCyclePhase::ClassificationPending
            | CompletionReviewCyclePhase::InitialReviewPending
            | CompletionReviewCyclePhase::CorrectionPending
            | CompletionReviewCyclePhase::RereviewPending
            | CompletionReviewCyclePhase::Closed => {}
        }
    }
    for step in &mut document.plan {
        if step.status == StepStatus::Passed {
            step.status = StepStatus::Implemented;
        }
    }
}

fn invalidate_for_plan_change(document: &mut TaskEvidenceDocument) {
    document.evidence_epoch = document.evidence_epoch.saturating_add(1);
    document.desktop_activation_receipt = None;
    document.latest_generated_artifact_hashes.clear();
    document.completion = None;
}

fn task_is_tracked(document: &TaskEvidenceDocument) -> bool {
    !document.plan.is_empty()
        || !document.edit_receipts.is_empty()
        || document
            .command_receipts
            .iter()
            .any(|receipt| receipt.possible_mutation)
        || document.risks.iter().any(|risk| {
            matches!(
                risk.source.as_str(),
                "task_evidence_storage" | "completion_review_recovery"
            ) && !risk.resolved
        })
}

fn resolve_risk(document: &mut TaskEvidenceDocument, id: &str) {
    if let Some(risk) = document.risks.iter_mut().find(|risk| risk.id == id) {
        risk.resolved = true;
    }
}

fn unreadable_file_risk(path: &str, epoch: u64, source: &str) -> EvidenceRisk {
    EvidenceRisk {
        id: unreadable_file_risk_id(path),
        description: format!(
            "task-controlled file `{}` is unreadable and cannot be freshness-checked",
            normalize_slashes(path)
        ),
        source: source.to_string(),
        blocking: false,
        resolved: false,
        epoch,
    }
}

fn task_evidence_storage_risk(reason: &str, epoch: u64) -> EvidenceRisk {
    EvidenceRisk {
        id: "task-evidence-storage-failure".to_string(),
        description: format!("task-evidence storage is unavailable: {reason}"),
        source: "task_evidence_storage".to_string(),
        blocking: false,
        resolved: false,
        epoch,
    }
}

fn task_evidence_recovery_risk(reason: &str, epoch: u64) -> EvidenceRisk {
    EvidenceRisk {
        id: "completion-review-lineage-recovery-failure".to_string(),
        description: format!("completion-review lineage could not be recovered: {reason}"),
        source: "completion_review_recovery".to_string(),
        blocking: false,
        resolved: false,
        epoch,
    }
}

fn unreadable_file_risk_id(path: &str) -> String {
    let digest = sha1_hex(normalize_slashes(path).as_bytes());
    format!("unreadable-file-{}", &digest[..16])
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
    match sha1_file(&absolute).await {
        Ok(sha1) => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: Some(sha1),
            exists: true,
            read_error: None,
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: None,
            exists: false,
            read_error: None,
        },
        Err(err) => FileHashSnapshot {
            path: normalize_slashes(normalized),
            sha1: None,
            exists: tokio::fs::symlink_metadata(&absolute).await.is_ok(),
            read_error: Some(format!("{:?}", err.kind())),
        },
    }
}

async fn snapshot_generated_artifact(repo_root: &Path, normalized: &str) -> FileHashSnapshot {
    let normalized = normalize_slashes(normalized);
    let path = Path::new(&normalized);
    if path.is_absolute() {
        return rejected_generated_artifact_snapshot(&normalized, "AbsoluteArtifactPath");
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return rejected_generated_artifact_snapshot(&normalized, "PathTraversalOutsideRepository");
    }
    let absolute = repo_root.join(path);
    if !generated_artifact_path_is_contained(repo_root, &absolute) {
        return rejected_generated_artifact_snapshot(&normalized, "OutsideRepository");
    }
    snapshot_file(repo_root, &normalized).await
}

fn generated_artifact_path_is_contained(repo_root: &Path, candidate: &Path) -> bool {
    let Ok(canonical_repo_root) = dunce::canonicalize(repo_root) else {
        return false;
    };
    let mut existing_ancestor = candidate.to_path_buf();
    loop {
        match dunce::canonicalize(&existing_ancestor) {
            Ok(canonical_candidate) => {
                return canonical_candidate == canonical_repo_root
                    || canonical_candidate
                        .strip_prefix(&canonical_repo_root)
                        .is_ok();
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if !existing_ancestor.pop() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

fn rejected_generated_artifact_snapshot(normalized: &str, reason: &str) -> FileHashSnapshot {
    FileHashSnapshot {
        path: normalize_slashes(normalized),
        sha1: None,
        exists: false,
        read_error: Some(reason.to_string()),
    }
}

async fn sha1_file(path: &Path) -> io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut buffer = vec![0_u8; FILE_HASH_CHUNK_SIZE];
    loop {
        let bytes_read = file.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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
        let codex_home = temp.path().join("home");
        tokio::fs::create_dir_all(repo.join(".git"))
            .await
            .expect("git dir");
        tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
            .await
            .expect("manifest");
        let cwd = AbsolutePathBuf::from_absolute_path(&repo).expect("absolute repo");
        let ledger =
            TaskEvidenceLedger::load_or_new(codex_home, ThreadId::new(), cwd.as_path()).await;
        (temp, ledger)
    }

    fn text_source(text: &str) -> UserInput {
        UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }
    }

    async fn source_dossier(
        ledger: &TaskEvidenceLedger,
        candidate: Option<&str>,
    ) -> CompletionReviewDossier {
        ledger
            .completion_review_dossier(
                candidate,
                &[],
                &[],
                &ReviewLensSelectionFacts::default(),
                &[],
                true,
                true,
            )
            .await
            .expect("completion review dossier")
    }

    fn text_local_classification(requirement_spans: Vec<SourceSpan>) -> SourceLocalClassification {
        SourceLocalClassification {
            local_kind: SourceLocalClassificationKind::RequirementBearing,
            local_semantic_cues: requirement_spans
                .iter()
                .cloned()
                .map(|source_span| LocalSemanticCue {
                    kind: LocalSemanticCueKind::Assertion,
                    source_span: Some(source_span),
                })
                .collect(),
            requirement_spans,
            reason: "immutable source contains requirements".to_string(),
        }
    }

    fn resolved_requirement_source(
        source: &UserSourceRecord,
        requirement_spans: &[SourceSpan],
    ) -> ClassifiedSource {
        ClassifiedSource {
            source_id: source.source_id.clone(),
            kind: ClassifiedSourceKind::RequirementBearing,
            requirements: requirement_spans
                .iter()
                .cloned()
                .map(|source_span| ClassifiedRequirement {
                    source_span,
                    status: RequirementStatus::Active,
                    superseded_by: None,
                })
                .collect(),
            reason: None,
        }
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
    async fn legacy_completed_is_canonicalized_to_passed() {
        let (_temp, ledger) = ledger_fixture().await;
        let normalized = ledger
            .record_plan_update(&plan(StepStatus::Completed))
            .await;
        assert_eq!(normalized.plan[0].status, StepStatus::Passed);
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Passed);
    }

    #[tokio::test]
    async fn missing_generation_and_desktop_activation_are_partial_conditions() {
        let (_temp, ledger) = ledger_fixture().await;
        let mut update = plan(StepStatus::Completed);
        update.plan[0].generated_artifacts = vec!["generated/missing.json".to_string()];
        update.plan[0].requires_desktop_activation = true;
        ledger.record_plan_update(&update).await;
        let gate = ledger.completion_gate().await.expect("gate");
        assert_eq!(gate.status, TaskCompletionStatus::Partial);
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
        let routes = completion_review_locally_obtainable_proof_routes(&gate);
        assert!(routes.iter().any(|route| {
            route.contains("native host implements a publish-ID-bound initialization handshake")
        }));
        assert!(
            routes
                .iter()
                .all(|route| !route.contains("Run the supported publish/restart route"))
        );
    }

    #[tokio::test]
    async fn finalization_advisory_is_bounded_and_read_only() {
        let (_temp, ledger) = ledger_fixture().await;
        ledger
            .record_plan_update(&plan(StepStatus::InProgress))
            .await;
        let revision = ledger
            .document
            .lock()
            .await
            .as_ref()
            .expect("document")
            .revision;
        let warning = ledger.finalization_advisory().await.expect("warning");
        assert!(!warning.contains("No automatic repair turn was started"));
        assert!(ledger.finalization_advisory().await.is_some());
        assert_eq!(
            ledger
                .document
                .lock()
                .await
                .as_ref()
                .expect("document")
                .revision,
            revision
        );
    }

    fn repair_finding(id: &str, requirement_ids: &[&str]) -> CompletionReviewFindingReceipt {
        CompletionReviewFindingReceipt {
            finding_id: id.to_string(),
            requirement_ids: requirement_ids.iter().map(|id| (*id).to_string()).collect(),
            lens: "correctness".to_string(),
            contract_surface: "task-evidence".to_string(),
            severity: "blocking".to_string(),
            evidence: "focused evidence".to_string(),
            smallest_correction: "repair the bounded surface".to_string(),
            proof_route: "focused test".to_string(),
        }
    }

    fn repair_command(sequence: u64, identity: &str) -> RepairCommandDelta {
        RepairCommandDelta {
            sequence,
            receipt_id: format!("command-{sequence}"),
            command: format!("check-{sequence}"),
            cwd: "repo".to_string(),
            exit_code: Some(0),
            timed_out: false,
            implementation_identity: Some(identity.to_string()),
        }
    }

    fn repair_surface(identifier: &str) -> StructuredContractSurface {
        StructuredContractSurface {
            kind: "rust_symbol".to_string(),
            owner: "task_evidence".to_string(),
            identifier: identifier.to_string(),
        }
    }

    fn repair_baseline_fixture() -> RepairBaseline {
        RepairBaseline {
            path_states: vec![RepairPathState {
                path: "src/lib.rs".to_string(),
                exists: true,
                content_hash: Some("before".to_string()),
            }],
            command_sequence_high_water_mark: 2,
            command_bindings: vec![
                BaselineCommandBinding {
                    sequence: 1,
                    receipt_id: "command-1".to_string(),
                    implementation_identity: Some("before-identity".to_string()),
                },
                BaselineCommandBinding {
                    sequence: 2,
                    receipt_id: "command-2".to_string(),
                    implementation_identity: Some("before-identity".to_string()),
                },
            ],
            implementation_surfaces: vec![repair_surface("existing")],
            repair_scope: RepairScope {
                path_grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
                paths: vec![RepairPathScope::ExactFile {
                    path: "src/lib.rs".to_string(),
                }],
                surfaces: vec![repair_surface("existing"), repair_surface("permitted-new")],
                affected_requirement_ids: vec!["R1".to_string(), "R2".to_string()],
            },
            source_ledger_hash: "source".to_string(),
            requirement_manifest_hash: "manifest".to_string(),
            plan_structure_hash: "plan".to_string(),
            default_child_mutation_identities: Vec::new(),
            typed_mutation_identities: Vec::new(),
            external_evidence_ids: Vec::new(),
        }
    }

    #[test]
    fn affected_requirements_are_deterministic_and_fail_closed() {
        let active = ["R3", "R1", "R2"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let findings = vec![
            repair_finding("F1", &["R2", "R1", "R2"]),
            repair_finding("F2", &["R1"]),
        ];
        assert_eq!(
            derive_affected_requirement_ids(&active, &findings),
            Ok(vec!["R1".to_string(), "R2".to_string()])
        );
        assert_eq!(
            derive_affected_requirement_ids(&active, &[]),
            Ok(vec!["R1".to_string(), "R2".to_string(), "R3".to_string()])
        );
        assert_eq!(
            derive_affected_requirement_ids(&active, &[repair_finding("F3", &[])]),
            Err(RereviewFallbackReason::RequirementManifestChanged)
        );
        assert_eq!(
            derive_affected_requirement_ids(
                &active,
                &[repair_finding("F4", &["inactive-or-unknown"])]
            ),
            Err(RereviewFallbackReason::RequirementManifestChanged)
        );
    }

    #[test]
    fn repair_path_scope_uses_the_bounded_versioned_grammar() {
        assert_eq!(
            canonical_repair_path("src\\.\\lib.rs", false),
            Ok("src/lib.rs".to_string())
        );
        for invalid in ["../src/lib.rs", "C:\\src\\lib.rs", "/src/lib.rs"] {
            assert_eq!(
                canonical_repair_path(invalid, false),
                Err(RereviewFallbackReason::InvalidPath)
            );
        }

        let exact = RepairPathScope::ExactFile {
            path: "src/lib.rs".to_string(),
        };
        assert!(repair_scope_matches(&exact, "src/lib.rs").expect("exact scope"));
        assert!(!repair_scope_matches(&exact, "src/lib.rs.bak").expect("exact boundary"));
        let prefix = RepairPathScope::DirectoryPrefix {
            path: "src".to_string(),
        };
        assert!(repair_scope_matches(&prefix, "src/nested/lib.rs").expect("prefix scope"));
        assert!(!repair_scope_matches(&prefix, "src-other/lib.rs").expect("prefix boundary"));

        let single = RepairPathScope::GeneratedPattern {
            grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
            pattern: "generated/*/out.json".to_string(),
        };
        assert!(repair_scope_matches(&single, "generated/a/out.json").expect("wildcard"));
        assert!(
            !repair_scope_matches(&single, "generated/a/b/out.json").expect("bounded wildcard")
        );
        let recursive = RepairPathScope::GeneratedPattern {
            grammar_version: REPAIR_PATH_GRAMMAR_VERSION,
            pattern: "generated/**/out.json".to_string(),
        };
        assert!(
            repair_scope_matches(&recursive, "generated/a/b/c/d/e/f/g/h/out.json")
                .expect("bounded recursive wildcard")
        );
        assert!(
            !repair_scope_matches(&recursive, "generated/a/b/c/d/e/f/g/h/i/out.json")
                .expect("recursive wildcard limit")
        );
        for (version, pattern) in [
            (REPAIR_PATH_GRAMMAR_VERSION + 1, "generated/*"),
            (REPAIR_PATH_GRAMMAR_VERSION, "generated/file?.json"),
            (REPAIR_PATH_GRAMMAR_VERSION, "generated/a*b.json"),
            (REPAIR_PATH_GRAMMAR_VERSION, "generated/**/**/out.json"),
        ] {
            assert_eq!(
                repair_scope_matches(
                    &RepairPathScope::GeneratedPattern {
                        grammar_version: version,
                        pattern: pattern.to_string(),
                    },
                    "generated/file1.json"
                ),
                Err(RereviewFallbackReason::UnsupportedPathGrammar)
            );
        }

        #[cfg(windows)]
        {
            assert_eq!(
                canonical_repair_path("SRC/Lib.rs", false),
                Ok("src/lib.rs".to_string())
            );
            assert_eq!(
                canonical_repair_path("src/é.rs", false),
                Err(RereviewFallbackReason::AmbiguousWindowsCase)
            );
        }
    }

    #[test]
    fn plan_status_is_non_structural_but_plan_contract_changes_are_structural() {
        let original = EvidencePlanStep {
            id: "implement".to_string(),
            step: "Implement the runtime path".to_string(),
            status: StepStatus::Implemented,
            depends_on: vec!["design".to_string()],
            acceptance_criteria: vec!["focused validation passes".to_string()],
            runtime_paths: vec!["src/lib.rs".to_string()],
            generated_artifacts: vec!["generated/schema.json".to_string()],
            risks: vec!["protocol".to_string()],
            requires_desktop_activation: false,
            edit_paths: BTreeSet::from(["src/lib.rs".to_string()]),
        };
        let mut status_only = original.clone();
        status_only.status = StepStatus::Passed;
        assert_eq!(
            plan_structure_hash(std::slice::from_ref(&original)),
            plan_structure_hash(std::slice::from_ref(&status_only))
        );

        let mut changed_text = original.clone();
        changed_text.step = "Change the runtime contract".to_string();
        assert_ne!(
            plan_structure_hash(std::slice::from_ref(&original)),
            plan_structure_hash(std::slice::from_ref(&changed_text))
        );
        let mut changed_paths = original.clone();
        changed_paths.edit_paths.insert("src/other.rs".to_string());
        assert_ne!(
            plan_structure_hash(std::slice::from_ref(&original)),
            plan_structure_hash(std::slice::from_ref(&changed_paths))
        );
    }

    #[test]
    fn contained_delta_uses_sequence_bindings_and_permitted_surfaces_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo source directory");
        std::fs::write(repo.join("src/lib.rs"), "after").expect("candidate source");
        let baseline = repair_baseline_fixture();
        let baseline_hash = repair_baseline_hash(&baseline);
        let current = CurrentRepairSnapshot {
            repository_root: repo.to_string_lossy().into_owned(),
            path_states: vec![RepairPathState {
                path: "src/lib.rs".to_string(),
                exists: true,
                content_hash: Some("after".to_string()),
            }],
            command_receipts: vec![
                repair_command(3, "candidate"),
                repair_command(1, "before-identity"),
                repair_command(2, "before-identity"),
            ],
            plan_structure_hash: "plan".to_string(),
            declared_path_scopes: baseline.repair_scope.paths.clone(),
            implementation_surfaces: vec![
                repair_surface("permitted-new"),
                repair_surface("existing"),
            ],
            default_child_mutation_identities: vec![
                serde_json::json!({ "paths": ["src/lib.rs"] }).to_string(),
            ],
            typed_mutation_identities: vec![
                serde_json::json!({ "paths": ["src/lib.rs"] }).to_string(),
            ],
            external_evidence_ids: Vec::new(),
            containment_errors: Vec::new(),
        };
        let original_findings = vec![repair_finding("F1", &["R1"])];
        let input = build_rereview_input(
            Some(&baseline),
            Some(&baseline_hash),
            Some("repair-hash"),
            &original_findings,
            &current,
            "candidate",
            "source",
            "manifest",
        )
        .expect("rereview input");
        assert_eq!(input.input_mode, RereviewInputMode::Delta);
        assert!(input.fallback_reasons.is_empty());
        let delta = input.delta.expect("contained delta");
        assert_eq!(
            delta
                .new_command_receipts
                .iter()
                .map(|receipt| receipt.sequence)
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(
            delta
                .invalidated_command_receipts
                .iter()
                .map(|receipt| receipt.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(delta.path_changes.len(), 1);
        assert_eq!(
            delta.newly_realized_surfaces,
            vec![repair_surface("permitted-new")]
        );
        assert_eq!(input.delta_hash, Some(repair_delta_hash(&delta)));
        assert!(validate_repair_delta_contents(&delta, &baseline, &original_findings).is_ok());
        let mut tampered = delta;
        tampered.affected_requirement_ids.push("R2".to_string());
        assert!(validate_repair_delta_contents(&tampered, &baseline, &original_findings).is_err());
    }

    #[test]
    fn full_fallback_collects_all_reasons_in_stable_enum_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("repo source directory");
        let baseline = repair_baseline_fixture();
        let current = CurrentRepairSnapshot {
            repository_root: repo.to_string_lossy().into_owned(),
            path_states: vec![RepairPathState {
                path: "outside.rs".to_string(),
                exists: true,
                content_hash: Some("outside".to_string()),
            }],
            command_receipts: vec![
                repair_command(1, "before-identity"),
                repair_command(2, "before-identity"),
            ],
            plan_structure_hash: "changed-plan".to_string(),
            declared_path_scopes: baseline.repair_scope.paths.clone(),
            implementation_surfaces: vec![repair_surface("outside")],
            default_child_mutation_identities: Vec::new(),
            typed_mutation_identities: Vec::new(),
            external_evidence_ids: vec!["new-evidence".to_string()],
            containment_errors: vec![RereviewFallbackReason::UnrepresentableEvidenceChange],
        };
        let input = build_rereview_input(
            Some(&baseline),
            Some("wrong-baseline-hash"),
            Some("repair-hash"),
            &[repair_finding("F1", &["R1"])],
            &current,
            "candidate",
            "changed-source",
            "changed-manifest",
        )
        .expect("fallback input");
        assert_eq!(input.input_mode, RereviewInputMode::FullFallback);
        assert!(input.delta.is_none());
        assert!(input.delta_hash.is_none());
        assert_eq!(
            input.fallback_reasons,
            vec![
                RereviewFallbackReason::InvalidBaselineHash,
                RereviewFallbackReason::PathOutsideScope,
                RereviewFallbackReason::SourceIdentityChanged,
                RereviewFallbackReason::RequirementManifestChanged,
                RereviewFallbackReason::PlanStructureChanged,
                RereviewFallbackReason::ContractSurfaceOutsideScope,
                RereviewFallbackReason::UnrepresentableEvidenceChange,
            ]
        );
    }

    #[test]
    fn source_classification_cache_tolerates_bad_elements_and_invalidates_duplicate_keys() {
        #[derive(Deserialize)]
        struct CacheEnvelope {
            #[serde(deserialize_with = "deserialize_source_classification_cache")]
            source_classification_cache: Vec<SourceClassificationCacheEntry>,
        }

        let entry =
            |content_hash: String, start: usize, end: usize| SourceClassificationCacheEntry {
                contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
                source_kind: UserSourceKind::Text,
                content_hash,
                classification: text_local_classification(vec![SourceSpan::Text { start, end }]),
            };
        let duplicate = entry("1".repeat(64), 0, 1);
        let malformed_duplicate = entry("e".repeat(64), 0, 1);
        let later = entry("f".repeat(64), 1, 2);
        let earlier = entry("0".repeat(64), 0, 1);
        let decoded: CacheEnvelope = serde_json::from_value(serde_json::json!({
            "source_classification_cache": [
                later,
                {"not": "a cache entry"},
                duplicate.clone(),
                earlier,
                duplicate,
                malformed_duplicate,
                {
                    "contract_version": SOURCE_CLASSIFICATION_CONTRACT_VERSION,
                    "source_kind": "text",
                    "content_hash": "e".repeat(64),
                    "classification": {"local_kind": "not_a_kind"}
                },
            ]
        }))
        .expect("tolerant cache envelope");

        assert_eq!(decoded.source_classification_cache.len(), 2);
        assert_eq!(
            decoded
                .source_classification_cache
                .iter()
                .map(|entry| entry.content_hash.clone())
                .collect::<Vec<_>>(),
            vec!["0".repeat(64), "f".repeat(64)]
        );
    }

    #[test]
    fn persisted_local_projection_contains_no_dossier_relative_fields() {
        let entry = SourceClassificationCacheEntry {
            contract_version: SOURCE_CLASSIFICATION_CONTRACT_VERSION.to_string(),
            source_kind: UserSourceKind::Text,
            content_hash: "a".repeat(64),
            classification: text_local_classification(vec![SourceSpan::Text { start: 0, end: 9 }]),
        };
        let encoded = serde_json::to_string(&entry).expect("serialize cache entry");

        for forbidden in [
            "requirement_status",
            "superseded_by",
            "source_id",
            "requirement_id",
            "source_ordinal",
            "completion_epoch",
            "manifest_revision",
        ] {
            assert!(!encoded.contains(forbidden), "unexpected field {forbidden}");
        }
    }

    #[tokio::test]
    async fn duplicate_source_occurrences_share_one_cache_entry_but_materialize_separately() {
        let (_temp, ledger) = ledger_fixture().await;
        let source = text_source("Use YAML.");
        assert!(
            ledger
                .record_user_sources("message-1", &[source.clone()])
                .await
        );
        assert!(ledger.record_user_sources("message-2", &[source]).await);
        let dossier = source_dossier(&ledger, None).await;
        assert_eq!(dossier.sources.len(), 2);
        assert_eq!(
            source_classification_cache_key(&dossier.sources[0]),
            source_classification_cache_key(&dossier.sources[1])
        );
        let span = SourceSpan::Text { start: 0, end: 9 };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let materialization = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key,
                text_local_classification(vec![span.clone()]),
            )]),
            resolved_sources: dossier
                .sources
                .iter()
                .map(|source| resolved_requirement_source(source, std::slice::from_ref(&span)))
                .collect(),
        };

        assert_eq!(
            ledger
                .apply_source_classification(&dossier, materialization)
                .await,
            AtomicReviewTransition::Persisted(())
        );
        let refreshed = source_dossier(&ledger, None).await;
        assert_eq!(refreshed.source_classification_cache.len(), 1);
        assert_eq!(refreshed.requirements.len(), 2);
        assert_ne!(
            refreshed.requirements[0].requirement_id,
            refreshed.requirements[1].requirement_id
        );
        assert!(refreshed.mappings_classified);
    }

    #[tokio::test]
    async fn resolver_version_transition_can_change_status_without_rewriting_identity_or_history() {
        let (_temp, ledger) = ledger_fixture().await;
        assert!(
            ledger
                .record_user_sources("message-1", &[text_source("Use YAML.")])
                .await
        );
        let dossier = source_dossier(&ledger, None).await;
        let span = SourceSpan::Text { start: 0, end: 9 };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let initial = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key.clone(),
                text_local_classification(vec![span.clone()]),
            )]),
            resolved_sources: vec![resolved_requirement_source(
                &dossier.sources[0],
                std::slice::from_ref(&span),
            )],
        };
        assert_eq!(
            ledger.apply_source_classification(&dossier, initial).await,
            AtomicReviewTransition::Persisted(())
        );

        let mut transition_dossier = source_dossier(&ledger, None).await;
        let original_requirement = transition_dossier.requirements[0].clone();
        let original_manifest_revision = transition_dossier.manifest_revision;
        transition_dossier.relationship_resolution_current = false;
        transition_dossier.mappings_classified = false;
        let replacement = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key,
                text_local_classification(vec![span.clone()]),
            )]),
            resolved_sources: vec![ClassifiedSource {
                source_id: transition_dossier.sources[0].source_id.clone(),
                kind: ClassifiedSourceKind::RequirementBearing,
                requirements: vec![ClassifiedRequirement {
                    source_span: span,
                    status: RequirementStatus::Withdrawn,
                    superseded_by: None,
                }],
                reason: None,
            }],
        };
        assert_eq!(
            ledger
                .apply_source_classification(&transition_dossier, replacement)
                .await,
            AtomicReviewTransition::Persisted(())
        );

        let refreshed = source_dossier(&ledger, None).await;
        assert_eq!(refreshed.manifest_revision, original_manifest_revision + 1);
        assert_eq!(refreshed.requirements.len(), 1);
        let rematerialized = &refreshed.requirements[0];
        assert_eq!(
            rematerialized.requirement_id,
            original_requirement.requirement_id
        );
        assert_eq!(rematerialized.source_id, original_requirement.source_id);
        assert_eq!(
            rematerialized.source_content_hash,
            original_requirement.source_content_hash
        );
        assert_eq!(rematerialized.source_span, original_requirement.source_span);
        assert_eq!(
            rematerialized.exact_material,
            original_requirement.exact_material
        );
        assert_eq!(rematerialized.status, RequirementStatus::Withdrawn);
        let guard = ledger.document.lock().await;
        let history = &guard
            .as_ref()
            .expect("document")
            .completion_review_v2
            .as_ref()
            .expect("completion review ledger")
            .manifest_snapshots;
        assert!(history.iter().any(|snapshot| {
            snapshot.manifest_revision == original_manifest_revision
                && snapshot.requirements == vec![original_requirement.clone()]
        }));
    }

    #[tokio::test]
    async fn non_authoring_materialization_failure_is_atomic() {
        let (_temp, ledger) = ledger_fixture().await;
        assert!(
            ledger
                .record_user_sources("message-1", &[text_source("Use YAML.")])
                .await
        );
        let dossier = source_dossier(&ledger, None).await;
        let span = SourceSpan::Text { start: 0, end: 9 };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let mut resolved = resolved_requirement_source(&dossier.sources[0], &[]);
        resolved.kind = ClassifiedSourceKind::RequirementBearing;
        let invalid = SourceMaterialization {
            local_classifications: BTreeMap::from([(key, text_local_classification(vec![span]))]),
            resolved_sources: vec![resolved],
        };

        assert_eq!(
            ledger.apply_source_classification(&dossier, invalid).await,
            AtomicReviewTransition::Failed
        );
        let refreshed = source_dossier(&ledger, None).await;
        assert_eq!(refreshed.document_revision, dossier.document_revision);
        assert!(refreshed.source_classification_cache.is_empty());
        assert!(refreshed.requirements.is_empty());
        assert!(!refreshed.mappings_classified);
    }

    #[tokio::test]
    async fn manifest_gap_correction_updates_cache_for_later_reconstruction() {
        let (_temp, ledger) = ledger_fixture().await;
        let material = "Use JSON. Encrypt it.";
        assert!(
            ledger
                .record_user_sources("message-1", &[text_source(material)])
                .await
        );
        let dossier = source_dossier(&ledger, None).await;
        let first = SourceSpan::Text { start: 0, end: 9 };
        let omitted = SourceSpan::Text {
            start: 10,
            end: material.len(),
        };
        let key = source_classification_cache_key(&dossier.sources[0]);
        let initial = SourceMaterialization {
            local_classifications: BTreeMap::from([(
                key.clone(),
                text_local_classification(vec![first.clone()]),
            )]),
            resolved_sources: vec![resolved_requirement_source(
                &dossier.sources[0],
                std::slice::from_ref(&first),
            )],
        };
        assert_eq!(
            ledger.apply_source_classification(&dossier, initial).await,
            AtomicReviewTransition::Persisted(())
        );

        let classified = source_dossier(&ledger, None).await;
        let gaps = vec![ManifestGapInput {
            source_id: classified.sources[0].source_id.clone(),
            omitted_spans: vec![omitted.clone()],
        }];
        let corrected = source_local_classifications_with_manifest_gaps(&classified, &gaps)
            .expect("corrected local facts");
        assert_eq!(
            corrected
                .get(&key)
                .expect("corrected key")
                .requirement_spans,
            vec![first.clone(), omitted.clone()]
        );
        let replacement = SourceMaterialization {
            local_classifications: corrected,
            resolved_sources: vec![resolved_requirement_source(
                &classified.sources[0],
                &[first, omitted.clone()],
            )],
        };
        assert_eq!(
            ledger
                .apply_source_classification(&classified, replacement)
                .await,
            AtomicReviewTransition::Persisted(())
        );

        let reconstructed = source_dossier(&ledger, None).await;
        assert_eq!(reconstructed.requirements.len(), 2);
        assert!(
            reconstructed
                .source_classification_cache
                .get(&key)
                .expect("persisted corrected cache entry")
                .requirement_spans
                .contains(&omitted)
        );
    }

    #[test]
    fn symlink_escape_is_uncontained() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&repo).expect("repo directory");
        std::fs::create_dir_all(&outside).expect("outside directory");
        std::fs::write(outside.join("file.rs"), "outside").expect("outside file");
        let link = repo.join("link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("directory symlink");
        #[cfg(windows)]
        if let Err(err) = std::os::windows::fs::symlink_dir(&outside, &link) {
            if err.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("directory symlink: {err}");
        }

        assert_eq!(
            path_resolves_within_repository(&repo.to_string_lossy(), "link/file.rs"),
            Err(RereviewFallbackReason::SymlinkEscape)
        );
    }
}

#[cfg(test)]
#[path = "task_evidence_tests.rs"]
mod hardening_tests;
