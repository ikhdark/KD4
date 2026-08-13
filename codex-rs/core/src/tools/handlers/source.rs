use crate::function_tool::FunctionCallError;
use crate::session::reasoning_governor::OwnerEvidenceReceiptV2;
use crate::session::reasoning_governor::SourceClosureReceiptState as OwnerEvidenceClosureState;
use crate::session::reasoning_governor::SourceOwnerReceiptState as OwnerEvidenceOwnerState;
use crate::task_evidence::OwnerPacketPreview;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::command_output_artifact::read_exact_tool_output_artifact;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::source_closure::GitObservationReceipt;
use crate::tools::handlers::source_closure::ReadReceipt;
use crate::tools::handlers::source_closure::SearchReceipt;
use crate::tools::handlers::source_closure::SharedSourceClosureState;
use crate::tools::handlers::source_closure::SourceMetadataToken;
use crate::tools::handlers::source_closure::SourceQuestion;
use crate::tools::handlers::source_closure::SourceQuestionKind;
use crate::tools::handlers::source_spec::LOCATE_TASK_TOOL_NAME;
use crate::tools::handlers::source_spec::READ_FILE_SPAN_TOOL_NAME;
use crate::tools::handlers::source_spec::SEARCH_SOURCE_TOOL_NAME;
use crate::tools::handlers::source_spec::SourceToolOptions;
use crate::tools::handlers::source_spec::create_locate_task_tool;
use crate::tools::handlers::source_spec::create_read_file_span_tool;
use crate::tools::handlers::source_spec::create_search_source_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::SourceCoverageKey;
use crate::turn_diff_tracker::SourceCoverageRevision;
use crate::turn_diff_tracker::SourceLineInterval;
use crate::turn_timing::SourceDiscoveryTimingEvent;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceActorRegistration;
use codex_agent_task_store::WorkspaceManifestEntry;
use codex_agent_task_store::WorkspaceStrategy;
use codex_file_search::source_search::ReadFileSpanOutput;
use codex_file_search::source_search::SOURCE_READ_DEFAULT_LINES;
use codex_file_search::source_search::SOURCE_SEARCH_DEFAULT_MAX_MATCHES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_FILE_BYTES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_ROOTS;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_DEPTH;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_DIRECTORIES;
use codex_file_search::source_search::SOURCE_SEARCH_MAX_WALK_ENTRIES;
use codex_file_search::source_search::SourceIgnoreMatcher;
use codex_file_search::source_search::SourceSearchAccumulator;
use codex_file_search::source_search::SourceSearchOptions;
use codex_file_search::source_search::SourceSearchOutput;
use codex_file_search::source_search::read_file_span_from_bytes;
use codex_file_search::source_search::should_descend_source_path;
use codex_file_search::source_search::should_scan_source_file;
use codex_file_search::source_search::validate_read_file_span_bounds;
use codex_file_search::task_locator::LOCATE_TASK_MAX_FILES;
use codex_file_search::task_locator::LOCATE_TASK_MAX_SOURCE_BYTES;
use codex_file_search::task_locator::LocateTaskDecisionFacts;
use codex_file_search::task_locator::LocateTaskOutput;
use codex_file_search::task_locator::LocateTaskRequest;
use codex_file_search::task_locator::LocateTaskSourceSection;
use codex_file_search::task_locator::LocateTaskSourceSectionKind;
use codex_file_search::task_locator::LocateTaskSourceSectionState;
use codex_file_search::task_locator::locate_task_cancellable;
use codex_file_search::task_locator::resolve_owner_candidates;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FileMetadata;
use codex_file_system::FileSystemSandboxContext;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalRetentionPolicy;
use codex_tools::CanonicalToolResult;
use codex_tools::ToolName;
use codex_tools::ToolOutputDiagnosticClass;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputProjectionFragment;
use codex_tools::ToolOutputProjectionFragmentKind;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::future::Future;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;
use tracing::warn;

#[cfg(test)]
pub(crate) mod test_observation {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Clone, Default)]
    struct Counters {
        successful_content_reads: Arc<AtomicUsize>,
        runtime_entries: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Snapshot {
        pub successful_content_reads: usize,
        pub runtime_entries: usize,
    }

    tokio::task_local! {
        static COUNTERS: Counters;
    }

    pub(crate) async fn observe<F: Future>(future: F) -> (F::Output, Snapshot) {
        let counters = Counters::default();
        let output = COUNTERS.scope(counters.clone(), future).await;
        let snapshot = Snapshot {
            successful_content_reads: counters.successful_content_reads.load(Ordering::Relaxed),
            runtime_entries: counters.runtime_entries.load(Ordering::Relaxed),
        };
        (output, snapshot)
    }

    pub(super) fn record_successful_content_read() {
        let _ = COUNTERS.try_with(|counters| {
            counters
                .successful_content_reads
                .fetch_add(1, Ordering::Relaxed);
        });
    }

    pub(super) fn record_runtime_entry() {
        let _ = COUNTERS.try_with(|counters| {
            counters.runtime_entries.fetch_add(1, Ordering::Relaxed);
        });
    }
}

const SOURCE_TOOL_MAX_RENDERED_BYTES: usize = 8 * 1024;
const SOURCE_COORDINATION_MAX_WAIT: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocateTaskArgs {
    task: String,
    #[serde(default)]
    path_anchor: Option<String>,
    #[serde(default)]
    symbol_anchor: Option<String>,
    #[serde(default)]
    max_files: Option<usize>,
    #[serde(default)]
    max_source_bytes: Option<usize>,
    #[serde(default)]
    force_fresh: bool,
    #[serde(default)]
    source_question: Option<SourceQuestion>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchSourceArgs {
    query: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    context_lines: Option<usize>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    include_generated: bool,
    #[serde(default)]
    include_vendor: bool,
    #[serde(default)]
    include_locks: bool,
    #[serde(default)]
    hydrate_selected_span: Option<bool>,
    #[serde(default)]
    force_fresh: bool,
    #[serde(default)]
    source_question: Option<SourceQuestion>,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileSpanArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    line_count: Option<usize>,
    /// Execute normally rather than reusing prior immutable evidence.
    #[serde(default)]
    force_fresh: bool,
    #[serde(default)]
    environment_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReadReplayArtifact {
    content_hash: String,
    output: ReadFileSpanOutput,
}

#[derive(Debug, Serialize, Deserialize)]
struct GitObservationArtifact {
    head: String,
    paths: Vec<String>,
    bounded_status: String,
    diff_head_sha256: String,
    revision_identity: String,
}

#[derive(Clone, Copy)]
enum SourceReservationKind {
    Read,
    Search,
}

struct SourceReservationGuard {
    state: SharedSourceClosureState,
    key: String,
    kind: SourceReservationKind,
    armed: bool,
}

impl SourceReservationGuard {
    fn new(state: SharedSourceClosureState, key: String, kind: SourceReservationKind) -> Self {
        Self {
            state,
            key,
            kind,
            armed: true,
        }
    }

    async fn finish(mut self) {
        finish_source_reservation(&self.state, &self.key, self.kind).await;
        self.armed = false;
    }
}

impl Drop for SourceReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let state = Arc::clone(&self.state);
        let key = self.key.clone();
        let kind = self.kind;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                finish_source_reservation(&state, &key, kind).await;
            });
        }
    }
}

async fn finish_source_reservation(
    state: &SharedSourceClosureState,
    key: &str,
    kind: SourceReservationKind,
) {
    let mut state = state.lock().await;
    match kind {
        SourceReservationKind::Read => state.finish_read_reservation(key),
        SourceReservationKind::Search => state.finish_search_reservation(key),
    }
}

const OWNER_EVIDENCE_BUNDLE_SCHEMA_VERSION: u32 = 2;
const OWNER_EVIDENCE_INLINE_TOKEN_TARGET: usize = 4_000;

#[derive(Clone, Debug, Serialize)]
struct ValidatedInstructionState {
    path: String,
    content_hash: String,
    source_snapshot_identity: String,
    applicability: &'static str,
    current: bool,
}

#[derive(Clone, Debug, Serialize)]
struct ValidationRouteDisposition {
    id: String,
    cwd: String,
    argv: Vec<String>,
    role: String,
    disposition: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct BundleUnresolvedQuestion {
    id: String,
    category: String,
    detail: String,
    next_evidence_action: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct ArtifactLineRange {
    start_line: usize,
    end_line: usize,
}

#[derive(Clone, Debug, Serialize)]
struct BundleSectionManifestEntry {
    section_id: String,
    category: String,
    state: &'static str,
    path: Option<String>,
    span: Option<codex_file_search::task_locator::ExactSpan>,
    content_hash: Option<String>,
    source_snapshot_identity: Option<String>,
    artifact_line_range: Option<ArtifactLineRange>,
}

#[derive(Clone, Debug, Serialize)]
struct BundleCategoryDisposition {
    category: &'static str,
    disposition: &'static str,
    blocking_unresolved_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct OwnerEvidenceBundleV2 {
    schema_version: u32,
    receipt: OwnerEvidenceReceiptV2,
    source_closure: LocateTaskDecisionFacts,
    applicable_instructions: Vec<ValidatedInstructionState>,
    validation_routes: Vec<ValidationRouteDisposition>,
    unresolved_questions: Vec<BundleUnresolvedQuestion>,
    category_dispositions: Vec<BundleCategoryDisposition>,
    section_manifest: Vec<BundleSectionManifestEntry>,
    next_action: &'static str,
}

#[derive(Clone, Debug)]
struct MaterializedBundleSection {
    section_id: String,
    category: String,
    path: Option<String>,
    span: Option<codex_file_search::task_locator::ExactSpan>,
    content_hash: String,
    source_snapshot_identity: Option<String>,
    text: String,
    projection_kind: ToolOutputProjectionFragmentKind,
}

struct OwnerEvidenceToolOutput {
    inner: FunctionToolOutput,
    projection: ToolOutputProjectionMetadata,
    signal: serde_json::Value,
    materialized_section_counts: [u32; 7],
    inline_section_counts: [u32; 7],
    avoided_singleton_reads: u32,
    closure_state: OwnerEvidenceClosureState,
}

struct SourceReadToolOutput {
    inner: FunctionToolOutput,
    canonical: CanonicalToolResult,
    projection: ToolOutputProjectionMetadata,
}

struct SourceSearchToolOutput {
    inner: FunctionToolOutput,
    canonical: CanonicalToolResult,
    projection: ToolOutputProjectionMetadata,
}

impl ToolOutput for SourceReadToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn sampling_request_signal(&self) -> Option<serde_json::Value> {
        self.inner.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.inner.deterministic_continuation_receipts()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        Some(self.projection.clone())
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(self.canonical.clone())
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> serde_json::Value {
        self.inner.code_mode_result(payload)
    }
}

impl ToolOutput for SourceSearchToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn sampling_request_signal(&self) -> Option<serde_json::Value> {
        self.inner.sampling_request_signal()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        Some(self.projection.clone())
    }

    fn canonical_result(&self, _payload: &ToolPayload) -> Option<CanonicalToolResult> {
        Some(self.canonical.clone())
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> serde_json::Value {
        self.inner.code_mode_result(payload)
    }
}

fn source_read_tool_output(
    output: ReadFileSpanOutput,
    rendered: String,
    signal: serde_json::Value,
    receipt: Option<TurnTimingDeterministicContinuationReceipt>,
    timing: &crate::turn_timing::TurnTimingState,
) -> SourceReadToolOutput {
    if output.requested_start_line > 1
        || output
            .end_line
            .is_some_and(|end_line| end_line < output.total_lines)
    {
        timing.record_strict_subset_source_reread();
    }
    let fragments = output
        .chunks
        .iter()
        .filter_map(|chunk| {
            output
                .exact_content
                .get(chunk.byte_start..chunk.byte_end)
                .map(|text| {
                    ToolOutputProjectionFragment::new(
                        ToolOutputProjectionFragmentKind::CitationOrExactSpan,
                        text,
                    )
                    .with_id(chunk.id.clone())
                })
        })
        .collect::<Vec<_>>();
    let projection = ToolOutputProjectionMetadata {
        outcome: ToolOutputOutcome::Success,
        diagnostic_class: ToolOutputDiagnosticClass::Normal,
        fragments,
        spillable_text: vec![rendered.clone()],
        essential_inline: json!({
            "path": &output.path,
            "full_file_sha256": &output.full_file_sha256,
            "requested_content_sha256": &output.requested_content_sha256,
            "requested_start_line": output.requested_start_line,
            "requested_line_count": output.requested_line_count,
            "available_start_line": output.start_line,
            "available_end_line": output.end_line,
            "total_lines": output.total_lines,
            "requested_bytes": output.requested_bytes,
            "complete": !output.truncated,
        }),
        requested_limit: None,
        predetermined_ranges: Vec::new(),
    };
    let canonical = CanonicalToolResult::text(output.exact_content)
        .with_retention_policy(CanonicalRetentionPolicy::ArtifactRequired);
    let mut inner =
        FunctionToolOutput::from_text(rendered, Some(true)).with_sampling_request_signal(signal);
    if let Some(receipt) = receipt {
        inner = inner.with_deterministic_continuation_receipt(receipt);
    }
    SourceReadToolOutput {
        inner,
        canonical,
        projection,
    }
}

impl ToolOutput for OwnerEvidenceToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn sampling_request_signal(&self) -> Option<serde_json::Value> {
        Some(self.signal.clone())
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        Some(self.projection.clone())
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        if self.avoided_singleton_reads == 0 {
            return Vec::new();
        }
        let resource = self
            .signal
            .get("receipt_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let state_revision = self
            .signal
            .get("closure_contract_revision")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        vec![TurnTimingDeterministicContinuationReceipt {
            class: DeterministicContinuationClass::SourceBundle,
            resource_identity_hash: sha256_text(resource),
            state_revision: state_revision.to_string(),
            host_action: DeterministicContinuationHostAction::BatchSourceBundle,
            suppressed_continuation_count: self.avoided_singleton_reads,
            avoided_token_usage: None,
        }]
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }
}

fn assemble_owner_evidence_bundle_v2(
    output: LocateTaskOutput,
    task_contract_epoch: &str,
    owner_packet: Option<&OwnerPacketPreview>,
    source_dependency_identity: Option<String>,
    validated_instruction_hashes: &BTreeMap<String, String>,
) -> OwnerEvidenceToolOutput {
    let mut source_closure = output.decision_facts.clone();
    let supporting_hashes = output
        .supporting_reads
        .iter()
        .map(|read| (read.path.replace('\\', "/"), read.content_hash.as_str()))
        .collect::<BTreeMap<_, _>>();
    let captured_instruction_sources = source_closure
        .captured_instruction_sources
        .iter()
        .map(|instruction| (instruction.path.replace('\\', "/"), instruction))
        .collect::<BTreeMap<_, _>>();
    let applicable_instructions = validated_instruction_hashes
        .iter()
        .map(|(path, validated_hash)| {
            let captured = captured_instruction_sources.get(path);
            let current = validated_instruction_identity_is_current(
                captured.copied(),
                &source_closure.source_snapshot_identity,
                validated_hash,
                supporting_hashes.get(path).copied(),
            );
            ValidatedInstructionState {
                path: path.clone(),
                content_hash: validated_hash.clone(),
                source_snapshot_identity: captured.map_or_else(String::new, |instruction| {
                    instruction.source_snapshot_identity.clone()
                }),
                applicability: "validated_model_visible_instruction_state",
                current,
            }
        })
        .collect::<Vec<_>>();
    let validation_routes = source_closure
        .candidate_validation_routes
        .iter()
        .map(|route| ValidationRouteDisposition {
            id: route.id.clone(),
            cwd: route.cwd.clone(),
            argv: route.argv.clone(),
            role: route.role.clone(),
            disposition: "available_not_executed",
        })
        .collect::<Vec<_>>();

    let authoritative_owner_id = source_closure
        .authoritative_owner
        .as_ref()
        .map(|owner| owner.id.clone());
    let owner_state = if authoritative_owner_id.is_some()
        && source_closure.unresolved_source_ambiguity.is_empty()
    {
        OwnerEvidenceOwnerState::OwnerResolved
    } else {
        OwnerEvidenceOwnerState::OwnerUnresolved
    };
    let mut unresolved_questions = Vec::new();
    for gap in &source_closure.source_gaps {
        push_bundle_question(
            &mut unresolved_questions,
            &source_closure.closure_contract_revision,
            "source_gap",
            gap,
            "Run one focused source evidence operation for this unresolved ID.",
        );
    }
    if let Some(owner_packet) = owner_packet {
        for obligation in &owner_packet.missing_obligations {
            push_bundle_question(
                &mut unresolved_questions,
                &source_closure.closure_contract_revision,
                "task_questions",
                obligation,
                "Resolve the named task or plan obligation before implementation closure.",
            );
        }
    }
    if source_closure.truncated {
        push_bundle_question(
            &mut unresolved_questions,
            &source_closure.closure_contract_revision,
            "source_materialization",
            "locator_closure_truncated",
            "Use one anchored locator or focused source operation for the omitted category.",
        );
    }
    for instruction in &applicable_instructions {
        if !instruction.current {
            push_bundle_question(
                &mut unresolved_questions,
                &source_closure.closure_contract_revision,
                "instructions",
                &format!("instruction_hash_mismatch:{}", instruction.path),
                "Refresh the locator snapshot before applying this instruction source.",
            );
        }
    }

    let mut materialized_sections = Vec::new();
    let mut section_manifest = Vec::new();
    for section in &source_closure.captured_source_sections {
        let identity_is_current = source_section_identity_is_current(
            section,
            &source_closure.source_snapshot_identity,
            &supporting_hashes,
        );
        if section.state == LocateTaskSourceSectionState::Materialized && identity_is_current {
            let text = section.text.clone().unwrap_or_default();
            materialized_sections.push(MaterializedBundleSection {
                section_id: section.section_id.clone(),
                category: source_section_category(section.kind).to_string(),
                path: Some(section.path.clone()),
                span: section.span.clone(),
                content_hash: section.content_hash.clone().unwrap_or_default(),
                source_snapshot_identity: Some(section.source_snapshot_identity.clone()),
                text,
                projection_kind: source_section_projection_kind(section.kind),
            });
        } else {
            let detail = if identity_is_current {
                format!("not_materialized:{}", section.section_id)
            } else {
                format!("source_identity_mismatch:{}", section.section_id)
            };
            push_bundle_question(
                &mut unresolved_questions,
                &source_closure.closure_contract_revision,
                source_section_category(section.kind),
                &detail,
                "Materialize this exact section from a single validated source snapshot.",
            );
            section_manifest.push(BundleSectionManifestEntry {
                section_id: section.section_id.clone(),
                category: source_section_category(section.kind).to_string(),
                state: "not_materialized",
                path: Some(section.path.clone()),
                span: section.span.clone(),
                content_hash: section.content_hash.clone(),
                source_snapshot_identity: Some(section.source_snapshot_identity.clone()),
                artifact_line_range: None,
            });
        }
    }

    append_core_section(
        &mut materialized_sections,
        &source_closure,
        "applicable_instructions",
        &applicable_instructions,
        ToolOutputProjectionFragmentKind::CoreInstructionOrTaskState,
    );
    append_core_section(
        &mut materialized_sections,
        &source_closure,
        "validation_routes",
        &validation_routes,
        ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary,
    );
    append_core_section(
        &mut materialized_sections,
        &source_closure,
        "unresolved_questions",
        &unresolved_questions,
        ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
    );
    if let Some(owner_packet) = owner_packet {
        append_core_section(
            &mut materialized_sections,
            &source_closure,
            "task_plan_state",
            owner_packet,
            ToolOutputProjectionFragmentKind::CoreInstructionOrTaskState,
        );
    }

    let closure_state = if owner_state == OwnerEvidenceOwnerState::OwnerResolved
        && unresolved_questions.is_empty()
        && required_source_categories_materialized(&source_closure)
    {
        OwnerEvidenceClosureState::BundleReady
    } else {
        OwnerEvidenceClosureState::BundleIncomplete
    };
    let receipt_id = stable_bundle_receipt_id(
        task_contract_epoch,
        authoritative_owner_id.as_deref(),
        &source_closure.source_snapshot_identity,
        &source_closure.closure_contract_revision,
    );
    let receipt = OwnerEvidenceReceiptV2 {
        receipt_id: receipt_id.clone(),
        task_contract_epoch: task_contract_epoch.to_string(),
        owner_id: authoritative_owner_id,
        source_snapshot_identity: source_closure.source_snapshot_identity.clone(),
        closure_contract_revision: source_closure.closure_contract_revision.clone(),
        owner_state,
        closure_state,
        unresolved_ids: unresolved_questions
            .iter()
            .map(|question| question.id.clone())
            .collect(),
    };
    let category_dispositions = category_dispositions(&source_closure, &unresolved_questions);

    // Source bytes live only in canonical section records. The model-visible
    // metadata carries identities and exact ranges, never a second copy.
    for section in &mut source_closure.captured_source_sections {
        section.text = None;
    }
    let next_action = directive_for_bundle_states(owner_state, closure_state);
    let mut bundle = OwnerEvidenceBundleV2 {
        schema_version: OWNER_EVIDENCE_BUNDLE_SCHEMA_VERSION,
        receipt,
        source_closure,
        applicable_instructions,
        validation_routes,
        unresolved_questions,
        category_dispositions,
        section_manifest: Vec::new(),
        next_action,
    };

    let active_categories = materialized_sections
        .iter()
        .map(|section| section.category.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        .max(1);
    let per_category_tokens = OWNER_EVIDENCE_INLINE_TOKEN_TARGET / active_categories;
    let mut remaining_by_category = BTreeMap::<String, usize>::new();
    let mut materialized_section_counts = [0_u32; 7];
    let mut inline_section_counts = [0_u32; 7];
    for (index, section) in materialized_sections.iter().enumerate() {
        let category_index = bundle_category_index(&section.category);
        materialized_section_counts[category_index] =
            materialized_section_counts[category_index].saturating_add(1);
        let tokens = section.text.len().div_ceil(4).max(1);
        let remaining = remaining_by_category
            .entry(section.category.clone())
            .or_insert(per_category_tokens);
        let state = if *remaining == 0 {
            "artifact_only"
        } else if tokens > *remaining {
            *remaining = 0;
            "partial"
        } else {
            *remaining -= tokens;
            "inline"
        };
        if state == "inline" || state == "partial" {
            inline_section_counts[category_index] =
                inline_section_counts[category_index].saturating_add(1);
        }
        section_manifest.push(BundleSectionManifestEntry {
            section_id: section.section_id.clone(),
            category: section.category.clone(),
            state,
            path: section.path.clone(),
            span: section.span.clone(),
            content_hash: Some(section.content_hash.clone()),
            source_snapshot_identity: section.source_snapshot_identity.clone(),
            artifact_line_range: Some(ArtifactLineRange {
                start_line: index + 3,
                end_line: index + 3,
            }),
        });
    }
    section_manifest.sort_by(|left, right| left.section_id.cmp(&right.section_id));
    bundle.section_manifest = section_manifest;

    let metadata_line = serde_json::to_string(&bundle).unwrap_or_else(|error| {
        json!({
            "schema_version": OWNER_EVIDENCE_BUNDLE_SCHEMA_VERSION,
            "receipt_id": receipt_id,
            "serialization_error": error.to_string(),
        })
        .to_string()
    });
    let mut artifact_lines = vec!["OWNER_EVIDENCE_BUNDLE_V2".to_string(), metadata_line];
    let mut fragments = Vec::new();
    for section in &materialized_sections {
        artifact_lines.push(
            json!({
                "section_id": section.section_id,
                "category": section.category,
                "path": section.path,
                "span": section.span,
                "content_hash": section.content_hash,
                "source_snapshot_identity": section.source_snapshot_identity,
                "exact_text": section.text,
            })
            .to_string(),
        );
        fragments.push(
            ToolOutputProjectionFragment::new(
                section.projection_kind,
                render_bundle_projection_fragment(section),
            )
            .with_id(section.section_id.clone()),
        );
    }
    let rendered = artifact_lines.join("\n");
    let essential_inline = serde_json::to_value(&bundle).unwrap_or_else(|_| {
        json!({
            "schema_version": OWNER_EVIDENCE_BUNDLE_SCHEMA_VERSION,
            "receipt_id": receipt_id,
            "owner_state": owner_state,
            "closure_state": closure_state,
            "next_action": next_action,
        })
    });
    let signal = json!({
        "kind": "source_evidence",
        "operation": "locate_task",
        "outcome": "success",
        "snapshot_id": bundle.receipt.source_snapshot_identity,
        "locator_request_identity": output.request_identity,
        "locator_reusable": bundle.source_closure.permits_exact_reuse(),
        "source_dependency_identity": source_dependency_identity,
        "receipt_id": bundle.receipt.receipt_id,
        "task_contract_epoch": bundle.receipt.task_contract_epoch,
        "closure_contract_revision": bundle.receipt.closure_contract_revision,
        "owner_state": owner_state,
        "closure_state": closure_state,
        "owner_id": bundle.receipt.owner_id,
        "primary_path": bundle.source_closure.primary_path,
        "materialized_paths": materialized_sections.iter().filter_map(|section| section.path.as_deref()).collect::<Vec<_>>(),
        "unresolved_ids": bundle.unresolved_questions.iter().map(|question| question.id.as_str()).collect::<Vec<_>>(),
        "validation_route": bundle.validation_routes.first().map(|route| route.id.as_str()),
        "relationship": {
            "kind": "known_advances",
            "obligations": if closure_state == OwnerEvidenceClosureState::BundleReady {
                vec!["owner", "governing_instructions", "caller_or_contract_closure", "focused_validation_route"]
            } else if owner_state == OwnerEvidenceOwnerState::OwnerResolved {
                vec!["owner", "governing_instructions"]
            } else {
                Vec::new()
            },
        },
        "advances": if closure_state == OwnerEvidenceClosureState::BundleReady {
            vec!["owner", "governing_instructions", "caller_or_contract_closure", "focused_validation_route"]
        } else if owner_state == OwnerEvidenceOwnerState::OwnerResolved {
            vec!["owner", "governing_instructions"]
        } else {
            Vec::new()
        },
        "introduces_uncertainty": closure_state == OwnerEvidenceClosureState::BundleIncomplete,
    });
    OwnerEvidenceToolOutput {
        inner: FunctionToolOutput::from_text(rendered.clone(), Some(true)),
        projection: ToolOutputProjectionMetadata {
            outcome: ToolOutputOutcome::Success,
            diagnostic_class: ToolOutputDiagnosticClass::Normal,
            fragments,
            spillable_text: vec![rendered],
            essential_inline,
            requested_limit: Some(OWNER_EVIDENCE_INLINE_TOKEN_TARGET),
            predetermined_ranges: Vec::new(),
        },
        signal,
        materialized_section_counts,
        inline_section_counts,
        avoided_singleton_reads: output
            .decision_facts
            .captured_source_sections
            .iter()
            .filter(|section| section.state == LocateTaskSourceSectionState::Materialized)
            .count()
            .saturating_sub(1) as u32,
        closure_state,
    }
}

fn bundle_category_index(category: &str) -> usize {
    match category {
        "primary_implementation" => 0,
        "direct_callers" => 1,
        "focused_tests" => 2,
        "contracts" => 3,
        "generated_relationships" => 4,
        "other_source_context" => 5,
        _ => 6,
    }
}

fn validated_instruction_identity_is_current(
    captured: Option<&codex_file_search::task_locator::LocateTaskSourceIdentity>,
    expected_snapshot: &str,
    validated_hash: &str,
    supporting_hash: Option<&str>,
) -> bool {
    captured.is_some_and(|instruction| {
        instruction.source_snapshot_identity == expected_snapshot
            && instruction.content_hash == validated_hash
            && supporting_hash == Some(validated_hash)
    })
}

fn push_bundle_question(
    questions: &mut Vec<BundleUnresolvedQuestion>,
    contract_revision: &str,
    category: &str,
    detail: &str,
    next_evidence_action: &str,
) {
    let id = sha256_fields(&[contract_revision, category, detail]);
    if questions.iter().any(|question| question.id == id) {
        return;
    }
    questions.push(BundleUnresolvedQuestion {
        id,
        category: category.to_string(),
        detail: detail.to_string(),
        next_evidence_action: next_evidence_action.to_string(),
    });
}

fn source_section_identity_is_current(
    section: &LocateTaskSourceSection,
    expected_snapshot: &str,
    supporting_hashes: &BTreeMap<String, &str>,
) -> bool {
    let Some(text) = section.text.as_deref() else {
        return false;
    };
    section.source_snapshot_identity == expected_snapshot
        && section
            .content_hash
            .as_deref()
            .is_some_and(|hash| hash == sha256_text(text))
        && section
            .file_content_hash
            .as_deref()
            .is_some_and(|file_hash| {
                supporting_hashes
                    .get(&section.path.replace('\\', "/"))
                    .is_some_and(|observed| *observed == file_hash)
            })
}

fn source_section_category(kind: LocateTaskSourceSectionKind) -> &'static str {
    match kind {
        LocateTaskSourceSectionKind::PrimaryImplementation => "primary_implementation",
        LocateTaskSourceSectionKind::Caller => "direct_callers",
        LocateTaskSourceSectionKind::Test => "focused_tests",
        LocateTaskSourceSectionKind::Contract => "contracts",
        LocateTaskSourceSectionKind::Generated => "generated_relationships",
        LocateTaskSourceSectionKind::OtherSourceContext => "other_source_context",
    }
}

fn source_section_projection_kind(
    kind: LocateTaskSourceSectionKind,
) -> ToolOutputProjectionFragmentKind {
    match kind {
        LocateTaskSourceSectionKind::PrimaryImplementation => {
            ToolOutputProjectionFragmentKind::SourcePrimaryImplementation
        }
        LocateTaskSourceSectionKind::Caller => ToolOutputProjectionFragmentKind::SourceCaller,
        LocateTaskSourceSectionKind::Test => ToolOutputProjectionFragmentKind::SourceTest,
        LocateTaskSourceSectionKind::Contract | LocateTaskSourceSectionKind::Generated => {
            ToolOutputProjectionFragmentKind::SourceContractOrGenerated
        }
        LocateTaskSourceSectionKind::OtherSourceContext => {
            ToolOutputProjectionFragmentKind::ContextualSpillableText
        }
    }
}

fn append_core_section<T: Serialize>(
    sections: &mut Vec<MaterializedBundleSection>,
    source_closure: &LocateTaskDecisionFacts,
    category: &str,
    value: &T,
    projection_kind: ToolOutputProjectionFragmentKind,
) {
    let Ok(text) = serde_json::to_string(value) else {
        return;
    };
    if text == "[]" || text == "null" {
        return;
    }
    let content_hash = sha256_text(&text);
    let section_id = sha256_fields(&[
        &source_closure.closure_contract_revision,
        category,
        "core",
        "no_path",
        "no_span",
        &content_hash,
        &source_closure.source_snapshot_identity,
    ]);
    sections.push(MaterializedBundleSection {
        section_id,
        category: category.to_string(),
        path: None,
        span: None,
        content_hash,
        source_snapshot_identity: None,
        text,
        projection_kind,
    });
}

fn required_source_categories_materialized(facts: &LocateTaskDecisionFacts) -> bool {
    if facts.primary_path.is_some()
        && !facts.captured_source_sections.iter().any(|section| {
            section.kind == LocateTaskSourceSectionKind::PrimaryImplementation
                && section.state == LocateTaskSourceSectionState::Materialized
        })
    {
        return false;
    }
    [
        (
            !facts.source_relationships.is_empty(),
            LocateTaskSourceSectionKind::Caller,
        ),
        (
            !facts.located_tests.is_empty(),
            LocateTaskSourceSectionKind::Test,
        ),
        (
            facts
                .located_contracts
                .iter()
                .any(|contract| contract.role == "contract"),
            LocateTaskSourceSectionKind::Contract,
        ),
        (
            facts
                .located_contracts
                .iter()
                .any(|contract| contract.role == "generated_mirror"),
            LocateTaskSourceSectionKind::Generated,
        ),
    ]
    .into_iter()
    .all(|(required, kind)| {
        !required
            || facts.captured_source_sections.iter().any(|section| {
                section.kind == kind && section.state == LocateTaskSourceSectionState::Materialized
            })
    })
}

fn category_dispositions(
    facts: &LocateTaskDecisionFacts,
    questions: &[BundleUnresolvedQuestion],
) -> Vec<BundleCategoryDisposition> {
    [
        "primary_implementation",
        "direct_callers",
        "focused_tests",
        "contracts",
        "generated_relationships",
        "instructions",
        "validation_routes",
        "task_questions",
    ]
    .into_iter()
    .map(|category| {
        let blocking_unresolved_ids = questions
            .iter()
            .filter(|question| {
                question.category == category
                    || (category == "task_questions" && question.category == "source_gap")
            })
            .map(|question| question.id.clone())
            .collect::<Vec<_>>();
        let has_material = match category {
            "primary_implementation" => facts.primary_path.is_some(),
            "direct_callers" => !facts.source_relationships.is_empty(),
            "focused_tests" => !facts.located_tests.is_empty(),
            "contracts" => facts
                .located_contracts
                .iter()
                .any(|entry| entry.role == "contract"),
            "generated_relationships" => facts
                .located_contracts
                .iter()
                .any(|entry| entry.role == "generated_mirror"),
            "instructions" => !facts.captured_instruction_sources.is_empty(),
            "validation_routes" => !facts.candidate_validation_routes.is_empty(),
            "task_questions" => !facts.source_gaps.is_empty(),
            _ => false,
        };
        BundleCategoryDisposition {
            category,
            disposition: if !blocking_unresolved_ids.is_empty() {
                "unresolved"
            } else if has_material {
                "established"
            } else {
                "not_applicable"
            },
            blocking_unresolved_ids,
        }
    })
    .collect()
}

fn directive_for_bundle_states(
    owner_state: OwnerEvidenceOwnerState,
    closure_state: OwnerEvidenceClosureState,
) -> &'static str {
    match (owner_state, closure_state) {
        (OwnerEvidenceOwnerState::OwnerResolved, OwnerEvidenceClosureState::BundleReady) => {
            "implementation_phase"
        }
        (_, OwnerEvidenceClosureState::BundleIncomplete) => "focused_evidence_followup",
        (OwnerEvidenceOwnerState::OwnerUnresolved, OwnerEvidenceClosureState::BundleReady) => {
            "owner_resolution_followup"
        }
    }
}

fn stable_bundle_receipt_id(
    task_contract_epoch: &str,
    owner_id: Option<&str>,
    snapshot_id: &str,
    closure_contract_revision: &str,
) -> String {
    sha256_fields(&[
        task_contract_epoch,
        owner_id.unwrap_or("owner_unresolved"),
        snapshot_id,
        closure_contract_revision,
        &OWNER_EVIDENCE_BUNDLE_SCHEMA_VERSION.to_string(),
    ])
}

fn render_bundle_projection_fragment(section: &MaterializedBundleSection) -> String {
    let citation = section
        .path
        .as_deref()
        .map(|path| {
            section.span.as_ref().map_or_else(
                || path.to_string(),
                |span| format!("{path}:{}-{}", span.start_line, span.end_line),
            )
        })
        .unwrap_or_else(|| section.category.clone());
    format!(
        "section_id: {}\ncitation: {}\ncontent_hash: {}\n{}",
        section.section_id, citation, section.content_hash, section.text
    )
}

fn sha256_text(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn sha256_fields(fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

pub struct LocateTaskHandler {
    options: SourceToolOptions,
    contract: SourceToolContract,
}

impl LocateTaskHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            options: SourceToolOptions {
                include_environment_id,
            },
            contract: SourceToolContract::new(create_locate_task_tool(SourceToolOptions {
                include_environment_id,
            })),
        }
    }
}

pub struct SearchSourceHandler {
    options: SourceToolOptions,
    contract: SourceToolContract,
}

impl SearchSourceHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            options: SourceToolOptions {
                include_environment_id,
            },
            contract: SourceToolContract::new(create_search_source_tool(SourceToolOptions {
                include_environment_id,
            })),
        }
    }
}

pub struct ReadFileSpanHandler {
    options: SourceToolOptions,
    contract: SourceToolContract,
}

impl ReadFileSpanHandler {
    pub(crate) fn new(include_environment_id: bool) -> Self {
        Self {
            options: SourceToolOptions {
                include_environment_id,
            },
            contract: SourceToolContract::new(create_read_file_span_tool(SourceToolOptions {
                include_environment_id,
            })),
        }
    }
}

struct SourceToolContract {
    spec: ToolSpec,
    validator: Result<jsonschema::Validator, String>,
}

impl SourceToolContract {
    fn new(spec: ToolSpec) -> Self {
        let validator = match &spec {
            ToolSpec::Function(tool) => serde_json::to_value(&tool.parameters)
                .map_err(|error| format!("failed to serialize emitted schema: {error}"))
                .and_then(|schema| {
                    jsonschema::validator_for(&schema)
                        .map_err(|error| format!("failed to compile emitted schema: {error}"))
                }),
            _ => Err("source tool contract is not a function schema".to_string()),
        };
        Self { spec, validator }
    }

    fn validate(&self, tool_name: &str, payload: &ToolPayload) -> Result<(), FunctionCallError> {
        let ToolPayload::Function { arguments } = payload else {
            return Err(FunctionCallError::RespondToModel(format!(
                "{tool_name} received unsupported payload"
            )));
        };
        let arguments = serde_json::from_str::<serde_json::Value>(arguments).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse {tool_name} arguments: {error}"
            ))
        })?;
        let validator = self.validator.as_ref().map_err(|error| {
            FunctionCallError::Fatal(format!(
                "{tool_name} emitted schema could not be used for preflight: {error}"
            ))
        })?;
        let mut errors = validator.iter_errors(&arguments);
        let Some(first) = errors.next() else {
            return Ok(());
        };
        let mut messages = vec![first.to_string()];
        messages.extend(errors.take(3).map(|error| error.to_string()));
        Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} arguments do not match its emitted schema: {}",
            messages.join("; ")
        )))
    }
}

impl ToolExecutor<ToolInvocation> for SearchSourceHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SEARCH_SOURCE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.contract.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.contract
                .validate(SEARCH_SOURCE_TOOL_NAME, &invocation.payload)?;
            handle_search_source(invocation, self.options).await
        })
    }
}

impl CoreToolRuntime for SearchSourceHandler {}

impl ToolExecutor<ToolInvocation> for LocateTaskHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LOCATE_TASK_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.contract.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.contract
                .validate(LOCATE_TASK_TOOL_NAME, &invocation.payload)?;
            handle_locate_task(invocation, self.options).await
        })
    }
}

impl CoreToolRuntime for LocateTaskHandler {}

impl ToolExecutor<ToolInvocation> for ReadFileSpanHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_FILE_SPAN_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.contract.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.contract
                .validate(READ_FILE_SPAN_TOOL_NAME, &invocation.payload)?;
            handle_read_file_span(invocation, self.options).await
        })
    }
}

impl CoreToolRuntime for ReadFileSpanHandler {}

async fn handle_locate_task(
    invocation: ToolInvocation,
    tool_options: SourceToolOptions,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    #[cfg(test)]
    test_observation::record_runtime_entry();
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "locate_task received unsupported payload".to_string(),
        ));
    };
    let args: LocateTaskArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse locate_task arguments: {err}"))
    })?;
    reject_unadvertised_environment_id(
        LOCATE_TASK_TOOL_NAME,
        tool_options,
        args.environment_id.as_deref(),
    )?;
    if args.task.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "locate_task requires a nonempty task description".to_string(),
        ));
    }
    let max_files = args.max_files.unwrap_or(LOCATE_TASK_MAX_FILES);
    if max_files == 0 || max_files > LOCATE_TASK_MAX_FILES {
        return Err(FunctionCallError::RespondToModel(format!(
            "locate_task max_files must be between 1 and {LOCATE_TASK_MAX_FILES}"
        )));
    }
    let max_source_bytes = args
        .max_source_bytes
        .unwrap_or(LOCATE_TASK_MAX_SOURCE_BYTES);
    if max_source_bytes == 0 || max_source_bytes > LOCATE_TASK_MAX_SOURCE_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "locate_task max_source_bytes must be between 1 and {LOCATE_TASK_MAX_SOURCE_BYTES}"
        )));
    }
    if let Some(question) = args.source_question.as_ref() {
        question
            .validate()
            .map_err(FunctionCallError::RespondToModel)?;
    }
    if args.source_question.is_none() {
        let closure = invocation.step_context.turn.source_closure.lock().await;
        if closure.locator_attempted {
            return Ok(boxed_tool_output(render_closure_preflight(
                LOCATE_TASK_TOOL_NAME,
                &closure,
                "locator_already_attempted",
                "The turn-scoped locator has already run. Use the existing closure evidence, or supply a concrete source_question for a newly discovered ambiguity.",
            )));
        }
    }
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_source_discovery(SourceDiscoveryTimingEvent::Locator);

    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    if let Some(question) = args.source_question.as_ref() {
        invocation
            .step_context
            .turn
            .source_closure
            .lock()
            .await
            .reopen_for_question(question);
    }
    let repository_root = source_context.repo_root_abs.as_path().to_path_buf();
    let cache_root = invocation
        .step_context
        .turn
        .config
        .codex_home
        .as_path()
        .to_path_buf();
    let manifest_path = repository_root.join("source_owners.toml");
    let environment_id = args.environment_id.clone();
    let task_for_packet = args.task.clone();
    let task = args.task;
    let path_anchor = args.path_anchor;
    let symbol_anchor = args.symbol_anchor;
    let force_fresh = args.force_fresh;
    let cancelled = Arc::new(AtomicBool::new(false));
    let blocking_cancelled = Arc::clone(&cancelled);
    let mut indexing_task = tokio::task::spawn_blocking(move || {
        locate_task_cancellable(
            &LocateTaskRequest {
                repository_root: &repository_root,
                cache_root: &cache_root,
                manifest_path: &manifest_path,
                environment_id: environment_id.as_deref(),
                task: &task,
                path_anchor: path_anchor.as_deref(),
                symbol_anchor: symbol_anchor.as_deref(),
                max_files,
                max_source_bytes,
                force_fresh,
            },
            &blocking_cancelled,
        )
    });
    let indexing_result = tokio::select! {
        result = &mut indexing_task => result,
        _ = invocation.cancellation_token.cancelled() => {
            cancelled.store(true, Ordering::Release);
            let _ = indexing_task.await;
            return Err(FunctionCallError::RespondToModel(
                "locate_task was cancelled".to_string(),
            ));
        }
    };
    let output = indexing_result
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("locate_task indexing task failed: {err}"))
        })?
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("locate_task query failed: {err:#}"))
        })?;

    let validated_instruction_hashes = invocation
        .step_context
        .loaded_agents_md
        .as_deref()
        .map(|instructions| {
            instructions
                .project_source_hashes()
                .into_iter()
                .filter_map(|(path, hash)| {
                    let absolute = path.to_abs_path().ok()?;
                    let relative = absolute
                        .strip_prefix(source_context.repo_root_abs.as_path())
                        .ok()?;
                    Some((relative.to_string_lossy().replace('\\', "/"), hash))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    let locator_artifact_id = if let Ok(serialized) = serde_json::to_vec(&json!({
        "request_identity": &output.request_identity,
        "snapshot_id": &output.snapshot_id,
        "decision_facts": &output.decision_facts,
        "rendered": &output.rendered,
    })) {
        store_source_replay_artifact(&invocation, &serialized).await
    } else {
        None
    };
    let owner_proven_for_snapshot = output.decision_facts.authoritative_owner.is_some();
    let owner_established = {
        let mut closure = invocation.step_context.turn.source_closure.lock().await;
        let owner_was_known = closure.summary().authoritative_owner.is_some();
        let authoritative_owner = output.decision_facts.authoritative_owner.clone();
        closure.apply_locator(&output.decision_facts, authoritative_owner);
        closure.locator_artifact_id = locator_artifact_id;
        !owner_was_known && closure.summary().authoritative_owner.is_some()
    };
    if owner_established {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::OwnerEstablished);
    }
    record_source_git_observation(&invocation, &source_context).await;
    let source_dependency_identity = invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .dependency_identity();
    if owner_proven_for_snapshot {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_pre_edit_owner_resolved();
    }
    let supporting_entries = output
        .supporting_reads
        .iter()
        .map(|read| WorkspaceManifestEntry {
            path: read.path.clone(),
            content_hash: Some(read.content_hash.clone()),
            existed: true,
        })
        .collect();
    record_supporting_source_reads(&invocation, &source_context, supporting_entries).await?;

    let owner_packet = {
        let outcome = invocation
            .session
            .services
            .task_evidence
            .record_owner_packet_from_locator(
                &output.decision_facts,
                output.owner_packet_seed.as_ref(),
                &output.supporting_reads,
                &output.request_identity,
                &task_for_packet,
            )
            .await;
        if let Some(outcome) = outcome.as_ref() {
            if outcome.created {
                invocation.step_context.turn.session_telemetry.counter(
                    "codex.owner_packet.created",
                    1,
                    &[],
                );
            }
            for (name, value) in [
                ("codex.owner_packet.files", outcome.files),
                ("codex.owner_packet.regions", outcome.regions),
                ("codex.owner_packet.callers", outcome.callers),
                (
                    "codex.owner_packet.acceptance_mappings",
                    outcome.acceptance_mappings,
                ),
            ] {
                invocation.step_context.turn.session_telemetry.histogram(
                    name,
                    i64::try_from(value).unwrap_or(i64::MAX),
                    &[],
                );
            }
        }
        outcome.map(|outcome| outcome.preview)
    };
    let task_contract_epoch = owner_packet.as_ref().map_or_else(
        || format!("turn_epoch:{}", invocation.turn.sub_id),
        |packet| format!("contract_epoch:{}", packet.contract_epoch),
    );

    let bundle_started = Instant::now();
    let bundle = assemble_owner_evidence_bundle_v2(
        output,
        &task_contract_epoch,
        owner_packet.as_ref(),
        source_dependency_identity,
        &validated_instruction_hashes,
    );
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_source_discovery(SourceDiscoveryTimingEvent::Bundle {
            generation_micros: bundle_started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            materialized: bundle.materialized_section_counts,
            inline: bundle.inline_section_counts,
            avoided_singleton_reads: bundle.avoided_singleton_reads,
        });
    if bundle.closure_state == OwnerEvidenceClosureState::BundleReady {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::ClosureEstablished);
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_pre_edit_implementation_ready();
    }
    Ok(boxed_tool_output(bundle))
}

async fn handle_search_source(
    invocation: ToolInvocation,
    tool_options: SourceToolOptions,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    #[cfg(test)]
    test_observation::record_runtime_entry();
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "search_source received unsupported payload".to_string(),
        ));
    };
    let args: SearchSourceArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse search_source arguments: {err}"))
    })?;
    reject_unadvertised_environment_id(
        SEARCH_SOURCE_TOOL_NAME,
        tool_options,
        args.environment_id.as_deref(),
    )?;
    if let Some(question) = args.source_question.as_ref() {
        question
            .validate()
            .map_err(FunctionCallError::RespondToModel)?;
    }
    let mut options = SourceSearchOptions::new(PathBuf::new(), args.query.clone());
    options.roots = args.paths.iter().map(PathBuf::from).collect();
    options.max_matches = args
        .max_results
        .unwrap_or(SOURCE_SEARCH_DEFAULT_MAX_MATCHES);
    options.context_lines = args.context_lines.unwrap_or(0);
    options.case_sensitive = args.case_sensitive;
    options.include_generated = args.include_generated;
    options.include_vendor = args.include_vendor;
    options.include_locks = args.include_locks;
    options.hydrate_selected_span = args.hydrate_selected_span.unwrap_or(true);
    options.hydration_candidates = invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .hydration_candidates();
    validate_search_root_count(&options.roots)?;
    let mut accumulator = SourceSearchAccumulator::new(&options)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    let (owner_known_before_search, closure_established_before_search) = {
        let closure = invocation.step_context.turn.source_closure.lock().await;
        (
            closure.summary().authoritative_owner.is_some(),
            closure.is_established(),
        )
    };
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_source_discovery(if owner_known_before_search {
            SourceDiscoveryTimingEvent::SearchAfterOwner
        } else {
            SourceDiscoveryTimingEvent::SearchBeforeOwner
        });
    if closure_established_before_search {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::PostClosureSearch {
                has_question: args.source_question.is_some(),
            });
    }
    {
        let mut closure = invocation.step_context.turn.source_closure.lock().await;
        if let Some(question) = args.source_question.as_ref() {
            closure.reopen_for_question(question);
        } else if closure.is_established() {
            if args.paths.is_empty() {
                invocation
                    .step_context
                    .turn
                    .turn_timing_state
                    .record_source_discovery(SourceDiscoveryTimingEvent::DuplicateSearchSuppressed);
                return Ok(boxed_tool_output(render_closure_preflight(
                    SEARCH_SOURCE_TOOL_NAME,
                    &closure,
                    "closure_established",
                    "Repository-wide rediscovery is suppressed. Supply bounded owner/target roots, or a concrete source_question that reopens ownership.",
                )));
            }
            if args.include_generated || args.include_vendor || args.include_locks {
                invocation
                    .step_context
                    .turn
                    .turn_timing_state
                    .record_source_discovery(SourceDiscoveryTimingEvent::DuplicateSearchSuppressed);
                return Ok(boxed_tool_output(render_closure_preflight(
                    SEARCH_SOURCE_TOOL_NAME,
                    &closure,
                    "closure_requires_narrowing",
                    "The requested filters broaden the established source surface. Supply a concrete source_question before including generated, vendor, or lock-file surfaces.",
                )));
            }
            if !closure.search_is_inside_closure(&args.paths) {
                let broad = args.paths.iter().any(|path| {
                    matches!(
                        path.trim().replace('\\', "/").as_str(),
                        "" | "." | "./" | "/"
                    )
                });
                let precise_bounded_file = args.paths.len() == 1
                    && !broad
                    && Path::new(&args.paths[0]).extension().is_some();
                if !precise_bounded_file {
                    invocation
                        .step_context
                        .turn
                        .turn_timing_state
                        .record_source_discovery(
                            SourceDiscoveryTimingEvent::DuplicateSearchSuppressed,
                        );
                    return Ok(boxed_tool_output(render_closure_preflight(
                        SEARCH_SOURCE_TOOL_NAME,
                        &closure,
                        "closure_requires_narrowing",
                        "The requested root ambiguously broadens beyond established ownership. Provide a precise file root or a concrete source_question.",
                    )));
                }
                let normalized = SourceQuestion {
                    kind: SourceQuestionKind::UnknownCaller,
                    detail: format!(
                        "bounded search `{}` under {}",
                        args.query,
                        args.paths.join(", ")
                    ),
                };
                closure.reopen_for_question(&normalized);
            }
        }
    }
    let recover_explicit_root_failures = !options.roots.is_empty();
    let roots = resolve_search_roots(&source_context, &options.roots).await?;
    let canonical_roots = roots
        .iter()
        .map(|root| relative_source_path(&source_context, root))
        .collect::<Result<Vec<_>, _>>()?;
    let ignore_matcher = SourceIgnoreMatcher::new_preloaded(
        source_context
            .is_git_repository
            .then_some(source_context.repo_root_abs.as_path()),
    );
    load_repository_exclude_rules(&source_context, &ignore_matcher).await?;
    // The filesystem executor does not currently expose the selected
    // environment's Git configuration or home directory. Reading the host's
    // values here mixes environments and can incorrectly hide results, so omit
    // global excludes explicitly until that contract is available.
    let omitted_global_ignore = source_context.is_git_repository;
    let scope_revision = complete_search_scope_revision(&source_context, &roots, &options).await?;
    let search_key = exact_search_key(
        &options,
        &canonical_roots,
        source_context.repo_root_abs.as_path(),
        &source_context.environment_id,
    );
    if let Some(scope_revision) = scope_revision.as_deref() {
        let mut closure = invocation.step_context.turn.source_closure.lock().await;
        if closure.search_scope_changed(&search_key, scope_revision) {
            closure.reopen_for_source_change(format!("search scope `{search_key}`"));
        }
    }
    // Exact turn coverage below stable-reads and hashes before reuse. The
    // older artifact replay path remains a compatibility reader, but no
    // longer decides whether source can be omitted from this context.
    let legacy_replay_enabled = false;
    if legacy_replay_enabled
        && !args.force_fresh
        && let Some(scope_revision) = scope_revision.as_deref()
    {
        let receipt = invocation
            .step_context
            .turn
            .source_closure
            .lock()
            .await
            .find_search(&search_key, scope_revision);
        if let Some(receipt) = receipt {
            let capped_zero = receipt.capped_zero;
            if let Some(bytes) =
                read_source_replay_artifact(&invocation, &receipt.artifact_id).await
                && let Ok(output) = serde_json::from_slice::<SourceSearchOutput>(&bytes)
            {
                {
                    let mut closure = invocation.step_context.turn.source_closure.lock().await;
                    apply_search_observations(&mut closure, &output, args.source_question.as_ref());
                }
                invocation
                    .step_context
                    .turn
                    .turn_timing_state
                    .record_source_discovery(SourceDiscoveryTimingEvent::SearchReused {
                        capped_zero,
                    });
                return Ok(boxed_tool_output(search_function_output(
                    &output,
                    omitted_global_ignore,
                    true,
                    &invocation.step_context.turn.turn_timing_state,
                )));
            }
        }
    }

    let reservation_key = scope_revision
        .as_ref()
        .map(|scope_revision| format!("{search_key}:{scope_revision}"));
    let reservation_guard = if args.force_fresh {
        None
    } else if let Some(reservation_key) = reservation_key.as_ref() {
        loop {
            let reservation = invocation
                .step_context
                .turn
                .source_closure
                .lock()
                .await
                .reserve_search(reservation_key.clone());
            match reservation {
                Ok(_) => {
                    break Some(SourceReservationGuard::new(
                        Arc::clone(&invocation.step_context.turn.source_closure),
                        reservation_key.clone(),
                        SourceReservationKind::Search,
                    ));
                }
                Err(mut waiter) => {
                    tokio::select! {
                        _ = waiter.changed() => {}
                        _ = invocation.cancellation_token.cancelled() => {
                            return Err(FunctionCallError::RespondToModel(
                                "search_source was cancelled".to_string(),
                            ));
                        }
                    }
                    if let Some(scope_revision) = scope_revision.as_deref() {
                        let receipt = invocation
                            .step_context
                            .turn
                            .source_closure
                            .lock()
                            .await
                            .find_search(&search_key, scope_revision);
                        if let Some(receipt) = receipt {
                            let capped_zero = receipt.capped_zero;
                            if let Some(bytes) =
                                read_source_replay_artifact(&invocation, &receipt.artifact_id).await
                                && let Ok(output) =
                                    serde_json::from_slice::<SourceSearchOutput>(&bytes)
                            {
                                {
                                    let mut closure =
                                        invocation.step_context.turn.source_closure.lock().await;
                                    apply_search_observations(
                                        &mut closure,
                                        &output,
                                        args.source_question.as_ref(),
                                    );
                                }
                                invocation
                                    .step_context
                                    .turn
                                    .turn_timing_state
                                    .record_source_discovery(
                                        SourceDiscoveryTimingEvent::SearchReused { capped_zero },
                                    );
                                return Ok(boxed_tool_output(search_function_output(
                                    &output,
                                    omitted_global_ignore,
                                    true,
                                    &invocation.step_context.turn.turn_timing_state,
                                )));
                            }
                        }
                    }
                }
            }
        }
    } else {
        None
    };

    let fresh_result = async {
        let mut observed_entries = BTreeMap::new();
        let traversal_started = Instant::now();
        let scan_result = scan_source_roots(
            &source_context,
            &roots,
            &options,
            &ignore_matcher,
            &mut accumulator,
            &mut observed_entries,
            recover_explicit_root_failures,
        )
        .await;
        accumulator.record_traversal_duration(traversal_started.elapsed());
        scan_result?;
        let output = accumulator.finish(canonical_roots);
        if !output.coverage_complete && output.matches.is_empty() {
            invocation
                .step_context
                .turn
                .turn_timing_state
                .record_source_discovery(SourceDiscoveryTimingEvent::CappedZeroResult);
        }
        let mut supporting_paths = output
            .matches
            .iter()
            .map(|matched| matched.path.clone())
            .collect::<Vec<_>>();
        supporting_paths.sort();
        supporting_paths.dedup();
        let supporting_entries = supporting_paths
            .into_iter()
            .map(|path| {
                observed_entries.remove(&path).ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "search_source: no read-time manifest was retained for matched path {path}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        record_supporting_source_reads(&invocation, &source_context, supporting_entries).await?;
        if let Some(scope_revision) = scope_revision.as_ref()
            && let Ok(serialized) = serde_json::to_vec(&output)
            && let Some(artifact_id) = store_source_replay_artifact(&invocation, &serialized).await
        {
            invocation
                .step_context
                .turn
                .source_closure
                .lock()
                .await
                .record_search(SearchReceipt {
                    key: search_key,
                    scope_revision: scope_revision.clone(),
                    artifact_id,
                    capped_zero: !output.coverage_complete && output.matches.is_empty(),
                });
        }
        {
            let mut closure = invocation.step_context.turn.source_closure.lock().await;
            apply_search_observations(&mut closure, &output, args.source_question.as_ref());
        }
        Ok::<_, FunctionCallError>(output)
    }
    .await;
    if let Some(guard) = reservation_guard {
        guard.finish().await;
    }
    let output = fresh_result?;
    Ok(boxed_tool_output(search_function_output(
        &output,
        omitted_global_ignore,
        false,
        &invocation.step_context.turn.turn_timing_state,
    )))
}

async fn handle_read_file_span(
    invocation: ToolInvocation,
    tool_options: SourceToolOptions,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    #[cfg(test)]
    test_observation::record_runtime_entry();
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_file_span received unsupported payload".to_string(),
        ));
    };
    let args: ReadFileSpanArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to parse read_file_span arguments: {err}"
        ))
    })?;
    reject_unadvertised_environment_id(
        READ_FILE_SPAN_TOOL_NAME,
        tool_options,
        args.environment_id.as_deref(),
    )?;
    let start_line = args.start_line.unwrap_or(1);
    let line_count = args.line_count.unwrap_or(SOURCE_READ_DEFAULT_LINES);
    validate_read_file_span_bounds(start_line, line_count)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_source_discovery(SourceDiscoveryTimingEvent::DirectReadRequested);
    if let Some((skill_path, bytes)) = read_loaded_skill_bytes(&invocation, &args.path).await? {
        let file_len = bytes.len();
        if file_len > SOURCE_SEARCH_MAX_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "source file `{}` is too large ({} bytes, max {})",
                args.path, file_len, SOURCE_SEARCH_MAX_FILE_BYTES
            )));
        }
        let output = read_file_span_from_bytes(skill_path, bytes, start_line, line_count)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let signal = read_file_span_signal(&output);
        let rendered = render_read_output(&output);
        return Ok(boxed_tool_output(source_read_tool_output(
            output,
            rendered,
            signal,
            None,
            &invocation.step_context.turn.turn_timing_state,
        )));
    }
    let source_context = local_source_context(&invocation, args.environment_id.as_deref()).await?;
    let path = resolve_confined_path(&source_context, &args.path, "source file").await?;
    let relative_path = relative_source_path(&source_context, &path)?;
    resolve_candidate_owner(
        &invocation,
        source_context.repo_root_abs.as_path(),
        relative_path.clone(),
    )
    .await;
    let metadata = source_context
        .fs
        .get_metadata(&path, Some(&source_context.sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", &path, err))?;
    if !metadata.is_file {
        return Err(FunctionCallError::RespondToModel(format!(
            "source path `{}` is not a file",
            args.path
        )));
    }
    let file_len = usize::try_from(metadata.size).unwrap_or(usize::MAX);
    if file_len > SOURCE_SEARCH_MAX_FILE_BYTES {
        return Err(FunctionCallError::RespondToModel(format!(
            "source file `{}` is too large ({} bytes, max {})",
            args.path, file_len, SOURCE_SEARCH_MAX_FILE_BYTES
        )));
    }
    let metadata_token =
        source_metadata_token(&invocation, &source_context, &path, &metadata).await;
    if let Some(metadata_token) = metadata_token.as_ref() {
        let requested_end = start_line.saturating_add(line_count.saturating_sub(1));
        let mut closure = invocation.step_context.turn.source_closure.lock().await;
        if closure.has_stale_read(&relative_path, start_line, requested_end, metadata_token) {
            closure.reopen_for_source_change(&relative_path);
        }
    }
    if !args.force_fresh
        && let Some(metadata_token) = metadata_token.as_ref()
        && let Some(output) = replay_covered_read(
            &invocation,
            &source_context,
            &path,
            &relative_path,
            start_line,
            line_count,
            metadata_token,
        )
        .await
    {
        invocation
            .step_context
            .turn
            .source_closure
            .lock()
            .await
            .mark_observed(&relative_path);
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::DirectReadReused);
        let signal = read_file_span_signal(&output);
        let rendered = render_read_output(&output);
        return Ok(boxed_tool_output(source_read_tool_output(
            output,
            rendered,
            signal,
            None,
            &invocation.step_context.turn.turn_timing_state,
        )));
    }

    // Exact turn coverage above is the authoritative reuse path. Keep the
    // older artifact replay reader disabled for ordinary reads.
    let legacy_replay_enabled = false;
    let (fragmented_replay, overlap_reused_lines) = if legacy_replay_enabled && !args.force_fresh {
        if let Some(metadata_token) = metadata_token.as_ref() {
            replay_fragmented_read(
                &invocation,
                &source_context,
                &path,
                &relative_path,
                start_line,
                line_count,
                metadata_token,
            )
            .await
        } else {
            (None, BTreeSet::new())
        }
    } else {
        (None, BTreeSet::new())
    };
    if let Some(output) = fragmented_replay {
        invocation
            .step_context
            .turn
            .source_closure
            .lock()
            .await
            .mark_observed(&relative_path);
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::DirectReadReused);
        let signal = read_file_span_signal(&output);
        let rendered = render_read_output(&output);
        return Ok(boxed_tool_output(source_read_tool_output(
            output,
            rendered,
            signal,
            None,
            &invocation.step_context.turn.turn_timing_state,
        )));
    }

    let reservation_key = crate::tools::handlers::source_closure::read_reservation_key(
        &relative_path,
        start_line,
        start_line.saturating_add(line_count.saturating_sub(1)),
    );
    let reservation_guard = if args.force_fresh {
        None
    } else {
        loop {
            let reservation = invocation
                .step_context
                .turn
                .source_closure
                .lock()
                .await
                .reserve_read(reservation_key.clone());
            match reservation {
                Ok(_) => {
                    break Some(SourceReservationGuard::new(
                        Arc::clone(&invocation.step_context.turn.source_closure),
                        reservation_key.clone(),
                        SourceReservationKind::Read,
                    ));
                }
                Err(mut waiter) => {
                    tokio::select! {
                        _ = waiter.changed() => {}
                        _ = invocation.cancellation_token.cancelled() => {
                            return Err(FunctionCallError::RespondToModel(
                                "read_file_span was cancelled".to_string(),
                            ));
                        }
                    }
                    let current_metadata = source_context
                        .fs
                        .get_metadata(&path, Some(&source_context.sandbox))
                        .await
                        .map_err(|err| source_fs_error("re-inspect", &path, err))?;
                    let current_token = source_metadata_token(
                        &invocation,
                        &source_context,
                        &path,
                        &current_metadata,
                    )
                    .await;
                    if legacy_replay_enabled
                        && let Some(current_token) = current_token.as_ref()
                        && let Some(output) = replay_covered_read(
                            &invocation,
                            &source_context,
                            &path,
                            &relative_path,
                            start_line,
                            line_count,
                            current_token,
                        )
                        .await
                    {
                        invocation
                            .step_context
                            .turn
                            .turn_timing_state
                            .record_source_discovery(SourceDiscoveryTimingEvent::DirectReadReused);
                        let signal = read_file_span_signal(&output);
                        let rendered = render_read_output(&output);
                        return Ok(boxed_tool_output(source_read_tool_output(
                            output,
                            rendered,
                            signal,
                            None,
                            &invocation.step_context.turn.turn_timing_state,
                        )));
                    }
                }
            }
        }
    };

    let fresh_result = async {
        let Some(bytes) = read_source_file_stably(&source_context, &path, &metadata).await? else {
            return Err(FunctionCallError::RespondToModel(format!(
                "source file `{}` changed while it was being read; retry the read",
                args.path
            )));
        };
        let content_hash = format!("{:x}", Sha256::digest(&bytes));
        let supporting_entry = manifest_entry_from_bytes(relative_path.clone(), &bytes);
        let mut output =
            read_file_span_from_bytes(relative_path.clone(), bytes.clone(), start_line, line_count)
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        record_supporting_source_reads(&invocation, &source_context, vec![supporting_entry])
            .await?;
        if let (Some(observed_start), Some(observed_end), Some(span_sha256)) = (
            output.start_line,
            output.end_line,
            hash_observed_source_span(&bytes, output.start_line, output.end_line),
        ) {
            invocation
                .session
                .services
                .task_evidence
                .record_owner_source_span(
                    &relative_path,
                    observed_start,
                    observed_end,
                    &content_hash,
                    &span_sha256,
                )
                .await;
        }
        let metadata_after = source_context
            .fs
            .get_metadata(&path, Some(&source_context.sandbox))
            .await
            .map_err(|err| source_fs_error("re-inspect", &path, err))?;
        let coverage_path = path.to_abs_path().map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "source file `{}` cannot be represented as a local coverage path: {err}",
                args.path
            ))
        })?;
        let coverage_decision = if let (Some(observed_start), Some(observed_end)) =
            (output.start_line, output.end_line)
        {
            let mut tracker = invocation.tracker.lock().await;
            let revision = SourceCoverageRevision {
                content_hash: content_hash.clone(),
                size: metadata_after.size,
                created_at_ms: metadata_after.created_at_ms,
                modified_at_ms: metadata_after.modified_at_ms,
                mutation_revision: tracker.current_mutation_revision(),
                compaction_epoch: tracker.current_compaction_epoch(),
            };
            let repo_root = source_context.repo_root_abs.as_path().to_string_lossy();
            let key = SourceCoverageKey::new(
                &source_context.environment_id,
                repo_root.as_ref(),
                coverage_path.as_ref(),
            );
            Some((
                revision.clone(),
                tracker.record_source_coverage(
                    key,
                    revision,
                    SourceLineInterval {
                        start_line: observed_start,
                        end_line: observed_end,
                    },
                    args.force_fresh,
                ),
            ))
        } else {
            None
        };
        let metadata_token =
            source_metadata_token(&invocation, &source_context, &path, &metadata_after).await;
        if let Some(metadata_token) = metadata_token
            && let Ok(serialized) = serde_json::to_vec(&ReadReplayArtifact {
                content_hash: content_hash.clone(),
                output: output.clone(),
            })
            && let Some(artifact_id) = store_source_replay_artifact(&invocation, &serialized).await
        {
            invocation
                .step_context
                .turn
                .source_closure
                .lock()
                .await
                .record_read(ReadReceipt {
                    path: relative_path.clone(),
                    start_line: output.start_line.unwrap_or(start_line),
                    end_line: output.end_line.unwrap_or(start_line),
                    metadata: metadata_token,
                    content_hash: content_hash.clone(),
                    artifact_id,
                });
        }
        invocation
            .step_context
            .turn
            .source_closure
            .lock()
            .await
            .mark_observed(&relative_path);
        let (coverage_receipt, reused_intervals) = coverage_decision.map_or_else(
            || (None, Vec::new()),
            |(revision, decision)| {
                let reused = decision.reused;
                let receipt = (!reused.is_empty()).then(|| {
                    let revision_text = json!({
                        "content_hash": revision.content_hash,
                        "size": revision.size,
                        "created_at_ms": revision.created_at_ms,
                        "modified_at_ms": revision.modified_at_ms,
                        "mutation_revision": revision.mutation_revision,
                        "compaction_epoch": revision.compaction_epoch,
                    })
                    .to_string();
                    TurnTimingDeterministicContinuationReceipt {
                        class: DeterministicContinuationClass::SourceCoverage,
                        resource_identity_hash: sha256_text(&format!(
                            "{}\0{}\0{}",
                            source_context.environment_id,
                            source_context.repo_root_abs.as_path().display(),
                            relative_path,
                        )),
                        state_revision: sha256_text(&revision_text),
                        host_action: if decision.missing.is_empty() {
                            DeterministicContinuationHostAction::ReuseCoveredSpan
                        } else {
                            DeterministicContinuationHostAction::ReadMissingRanges
                        },
                        suppressed_continuation_count: 1,
                        avoided_token_usage: None,
                    }
                });
                if decision.missing.is_empty() {
                    output.lines.clear();
                } else {
                    output.lines.retain(|line| {
                        decision.missing.iter().any(|interval| {
                            line.line_number >= interval.start_line
                                && line.line_number <= interval.end_line
                        })
                    });
                }
                output.start_line = output.lines.first().map(|line| line.line_number);
                output.end_line = output.lines.last().map(|line| line.line_number);
                output.bytes_returned = output
                    .lines
                    .iter()
                    .map(|line| line.text.len().saturating_add(1))
                    .sum();
                (receipt, reused)
            },
        );
        let new_lines = output.lines.iter().collect::<Vec<_>>();
        if !reused_intervals.is_empty() || !overlap_reused_lines.is_empty() {
            invocation
                .step_context
                .turn
                .turn_timing_state
                .record_source_discovery(SourceDiscoveryTimingEvent::OverlapTrimmedRead);
        }
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::NewRead {
                lines: u64::try_from(new_lines.len()).unwrap_or(u64::MAX),
                bytes: u64::try_from(
                    new_lines
                        .iter()
                        .map(|line| line.text.len().saturating_add(1))
                        .sum::<usize>(),
                )
                .unwrap_or(u64::MAX),
            });
        let mut rendered = if output.lines.is_empty() && coverage_receipt.is_some() {
            format!(
                "Source file evidence already present in current context.\npath: {relative_path}\nreused_intervals: {}",
                render_source_intervals(&reused_intervals),
            )
        } else {
            render_read_output(&output)
        };
        if !reused_intervals.is_empty() && !output.lines.is_empty() {
            rendered.push_str(&format!(
                "\nreused_intervals: {}",
                render_source_intervals(&reused_intervals),
            ));
        }
        let signal = read_file_span_signal(&output);
        Ok::<_, FunctionCallError>((output, rendered, signal, coverage_receipt))
    }
    .await;
    if let Some(guard) = reservation_guard {
        guard.finish().await;
    }
    let (output, rendered, signal, coverage_receipt) = fresh_result?;
    Ok(boxed_tool_output(source_read_tool_output(
        output,
        rendered,
        signal,
        coverage_receipt,
        &invocation.step_context.turn.turn_timing_state,
    )))
}

fn render_source_intervals(intervals: &[SourceLineInterval]) -> String {
    intervals
        .iter()
        .map(|interval| format!("{}-{}", interval.start_line, interval.end_line))
        .collect::<Vec<_>>()
        .join(",")
}

fn read_file_span_signal(output: &ReadFileSpanOutput) -> serde_json::Value {
    json!({
        "kind": "source_evidence",
        "operation": "read_file_span",
        "path": output.path,
        "source_map_route": output.source_map_route,
        "start_line": output.start_line,
        "end_line": output.end_line,
        "total_lines": output.total_lines,
        "truncated": output.truncated,
    })
}

fn apply_search_observations(
    closure: &mut crate::tools::handlers::source_closure::SourceClosureState,
    output: &SourceSearchOutput,
    question: Option<&SourceQuestion>,
) {
    let discovered_role = question.and_then(|question| match question.kind {
        SourceQuestionKind::UnknownCaller => Some("caller"),
        SourceQuestionKind::UnknownContract => Some("contract"),
        SourceQuestionKind::ValidationDependency => Some("validation_dependency"),
        SourceQuestionKind::AmbiguousOwnership
        | SourceQuestionKind::IncompletePriorResult
        | SourceQuestionKind::SourceChanged => None,
    });
    for matched in &output.matches {
        if let Some(role) = discovered_role {
            closure.record_discovered_target(&matched.path, role);
        } else {
            closure.mark_observed(&matched.path);
        }
    }
}

fn render_closure_preflight(
    operation: &str,
    state: &crate::tools::handlers::source_closure::SourceClosureState,
    disposition: &str,
    guidance: &str,
) -> FunctionToolOutput {
    let summary = state.summary();
    let owner_resolved = summary.authoritative_owner.is_some();
    let closure_ready = owner_resolved
        && summary.discovery
            == crate::tools::handlers::source_closure::SourceClosureDisposition::Established
        && summary.pending_required_targets.is_empty();
    let primary_path = summary.primary_implementation.first().cloned();
    let materialized_paths = summary
        .relevant_targets
        .iter()
        .filter(|target| target.established)
        .map(|target| target.path.clone())
        .collect::<Vec<_>>();
    let unresolved_ids = summary
        .pending_required_targets
        .iter()
        .chain(summary.unresolved_questions.iter())
        .cloned()
        .collect::<Vec<_>>();
    let rendered = serde_json::to_string_pretty(&json!({
        "source_closure": summary,
        "disposition": disposition,
        "guidance": guidance,
    }))
    .unwrap_or_else(|_| format!("source closure {disposition}; {guidance}"));
    FunctionToolOutput::from_text(rendered, Some(true)).with_sampling_request_signal(json!({
        "kind": "source_evidence",
        "operation": operation,
        "outcome": disposition,
        "source_disposition": disposition,
        "evidence_revision": state.source_revision,
        "snapshot_id": state.source_snapshot_identity.as_deref(),
        "receipt_id": format!("source-closure-preflight:{}", state.source_revision),
        "owner_state": if owner_resolved { "owner_resolved" } else { "owner_unresolved" },
        "closure_state": if closure_ready { "bundle_ready" } else { "bundle_incomplete" },
        "owner_id": summary.authoritative_owner.as_deref(),
        "primary_path": primary_path,
        "materialized_paths": materialized_paths,
        "unresolved_ids": unresolved_ids,
        "validation_route": (summary.validation == "known").then_some("source-closure-known"),
    }))
}

fn search_function_output(
    output: &SourceSearchOutput,
    omitted_global_ignore: bool,
    replayed: bool,
    timing: &crate::turn_timing::TurnTimingState,
) -> SourceSearchToolOutput {
    timing.record_search_index(output.coverage.index_complete);
    let signal = json!({
        "kind": "source_evidence",
        "operation": "search_source",
        "query": output.query.clone(),
        "roots": output.roots.clone(),
        "paths": output.matches.iter().map(|matched| matched.path.as_str()).collect::<Vec<_>>(),
        "truncated": output.truncated,
        "coverage_complete": output.coverage_complete,
        "hydration_status": output.hydration_status,
        "hydrated_path": output.hydrated_span.as_ref().map(|hydrated| hydrated.observation.path.as_str()),
        "source_disposition": if replayed { "exact_replay" } else { "fresh" },
    });
    let mut rendered = render_search_output(output);
    if !output.coverage_complete && output.matches.is_empty() {
        rendered.push_str(
            "\nnon_authoritative_zero: true\nguidance: narrow the bounded roots or change coverage; an unchanged ordinary retry reuses this incomplete disposition\n",
        );
    }
    if omitted_global_ignore {
        rendered.push_str(
            "\ndiagnostic: global Git ignore rules were omitted because the selected environment does not expose Git config resolution\n",
        );
    }
    let fragments = output
        .matches
        .iter()
        .filter_map(|matched| {
            let value = serde_json::to_value(matched).ok()?;
            let exact_match = CanonicalToolResult::json(value);
            let text = String::from_utf8(exact_match.bytes).ok()?;
            Some(
                ToolOutputProjectionFragment::new(
                    ToolOutputProjectionFragmentKind::SearchMatchOrDefinition,
                    text,
                )
                .with_id(matched.id.clone()),
            )
        })
        .collect::<Vec<_>>();
    let projection = ToolOutputProjectionMetadata {
        outcome: ToolOutputOutcome::Success,
        diagnostic_class: ToolOutputDiagnosticClass::Normal,
        fragments,
        spillable_text: vec![rendered.clone()],
        essential_inline: json!({
            "query": &output.query,
            "coverage_complete": output.coverage_complete,
            "index_complete": output.coverage.index_complete,
            "context_complete": output.coverage.context_complete,
            "indexed_matches": output.coverage.indexed_matches,
            "matches_returned": output.coverage.matches_returned,
            "omitted_contexts": output.coverage.omitted_contexts,
            "result_cap_reached": output.coverage.result_cap_reached,
            "match_ids": output.matches.iter().map(|matched| matched.id.as_str()).collect::<Vec<_>>(),
        }),
        requested_limit: None,
        predetermined_ranges: Vec::new(),
    };
    let canonical =
        CanonicalToolResult::json(serde_json::to_value(output).unwrap_or_else(|_| json!(null)));
    let inner =
        FunctionToolOutput::from_text(rendered, Some(true)).with_sampling_request_signal(signal);
    SourceSearchToolOutput {
        inner,
        canonical,
        projection,
    }
}

async fn store_source_replay_artifact(invocation: &ToolInvocation, bytes: &[u8]) -> Option<String> {
    let thread_id = invocation
        .session
        .services
        .agent_control
        .session_id()
        .to_string();
    match create_raw_output_artifact(
        invocation.step_context.turn.config.codex_home.as_path(),
        &thread_id,
        bytes,
    )
    .await
    {
        RawOutputArtifact::Stored {
            id,
            truncated: false,
            ..
        } => Some(id.to_string()),
        RawOutputArtifact::Stored {
            truncated: true, ..
        }
        | RawOutputArtifact::Failed { .. } => None,
    }
}

async fn read_source_replay_artifact(
    invocation: &ToolInvocation,
    artifact_id: &str,
) -> Option<Vec<u8>> {
    let thread_id = invocation
        .session
        .services
        .agent_control
        .session_id()
        .to_string();
    read_exact_tool_output_artifact(
        invocation.step_context.turn.config.codex_home.as_path(),
        &thread_id,
        artifact_id,
    )
    .await
    .ok()
}

async fn record_source_git_observation(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
) {
    if !source_context.is_git_repository {
        return;
    }
    let summary = invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .summary();
    let mut paths = summary.primary_implementation;
    paths.extend(
        summary
            .relevant_targets
            .into_iter()
            .map(|target| target.path),
    );
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return;
    }

    let Some(registration) = invocation
        .session
        .services
        .git_workspace
        .register_source_freshness_paths(
            paths
                .iter()
                .map(|path| source_context.repo_root_abs.join(path).to_path_buf()),
        )
    else {
        return;
    };
    let watcher_generation = registration.watcher_generation;
    let host_mutation_generation = registration.host_mutation_generation;
    invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .retain_source_watch("git-observation".to_string(), registration);
    let mutation_revision = invocation.tracker.lock().await.current_mutation_revision();
    let paths_freshness = source_paths_freshness_identity(invocation, source_context, &paths).await;
    let freshness_key = format!(
        "repo={};env={};paths={};watcher={watcher_generation};host={host_mutation_generation};mutation={mutation_revision};files={}",
        source_context.repo_root_abs.display(),
        source_context.environment_id,
        paths.join("\u{1f}"),
        paths_freshness.as_deref().unwrap_or("unreusable"),
    );
    let existing = invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .git_observation
        .clone();
    if existing
        .as_ref()
        .is_some_and(|existing| existing.freshness_key != freshness_key)
    {
        invocation
            .step_context
            .turn
            .source_closure
            .lock()
            .await
            .reopen_for_source_change("Git observation freshness");
    }
    if paths_freshness.is_some()
        && let Some(existing) = existing
        && existing.freshness_key == freshness_key
        && read_source_replay_artifact(invocation, &existing.artifact_id)
            .await
            .is_some()
    {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_source_discovery(SourceDiscoveryTimingEvent::GitObservationReused);
        return;
    }
    let head = run_bounded_git(source_context, &["rev-parse", "HEAD"], &[]).await;
    let status = run_bounded_git(
        source_context,
        &["status", "--porcelain=v1", "--untracked-files=all"],
        &paths,
    )
    .await;
    let diff = run_bounded_git(
        source_context,
        &["diff", "--no-ext-diff", "--binary", "HEAD"],
        &paths,
    )
    .await;
    let (Some(head), Some(status), Some(diff)) = (head, status, diff) else {
        return;
    };
    let head = String::from_utf8_lossy(&head).trim().to_string();
    let bounded_status = bounded_utf8(String::from_utf8_lossy(&status).trim(), 64 * 1024);
    let diff_head_sha256 = format!("{:x}", Sha256::digest(&diff));
    let revision_identity = format!(
        "repo={};env={};watcher={watcher_generation};host={host_mutation_generation};mutation={mutation_revision}",
        source_context.repo_root_abs.display(),
        source_context.environment_id,
    );
    let paths_freshness_after =
        source_paths_freshness_identity(invocation, source_context, &paths).await;
    let stable_freshness_key = if paths_freshness_after == paths_freshness {
        freshness_key
    } else {
        format!("unreusable:{mutation_revision}")
    };
    let artifact = GitObservationArtifact {
        head,
        paths,
        bounded_status,
        diff_head_sha256,
        revision_identity,
    };
    let Ok(serialized) = serde_json::to_vec(&artifact) else {
        return;
    };
    let identity = format!("{:x}", Sha256::digest(&serialized));
    let Some(artifact_id) = store_source_replay_artifact(invocation, &serialized).await else {
        return;
    };
    invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .git_observation = Some(GitObservationReceipt {
        freshness_key: stable_freshness_key,
        identity,
        artifact_id,
    });
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_source_discovery(SourceDiscoveryTimingEvent::GitObservationRefreshed);
}

async fn source_paths_freshness_identity(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    paths: &[String],
) -> Option<String> {
    let mut hasher = Sha256::new();
    for path in paths {
        let absolute = source_context.repo_root_abs.join(path);
        let uri = PathUri::from_abs_path(&absolute);
        let metadata = source_context
            .fs
            .get_metadata(&uri, Some(&source_context.sandbox))
            .await
            .ok()?;
        let token = source_metadata_token(invocation, source_context, &uri, &metadata).await?;
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(token.size.to_le_bytes());
        hasher.update(token.created_at_ms.to_le_bytes());
        hasher.update(token.modified_at_ms.to_le_bytes());
        hasher.update(token.mutation_revision.to_le_bytes());
        hasher.update(token.watcher_generation.to_le_bytes());
        hasher.update(token.host_mutation_generation.to_le_bytes());
        hasher.update(token.stable_file_identity.as_bytes());
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn bounded_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

async fn run_bounded_git(
    source_context: &LocalSourceContext,
    arguments: &[&str],
    paths: &[String],
) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .current_dir(source_context.repo_root_abs.as_path())
        .args(arguments);
    if !paths.is_empty() {
        command.arg("--").args(paths);
    }
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

async fn replay_covered_read(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    source_path: &PathUri,
    path: &str,
    start_line: usize,
    line_count: usize,
    metadata: &SourceMetadataToken,
) -> Option<ReadFileSpanOutput> {
    let end_line = start_line.saturating_add(line_count.saturating_sub(1));
    let receipt = invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .find_covering_read(path, start_line, end_line, metadata)?;
    let bytes = read_source_replay_artifact(invocation, &receipt.artifact_id).await?;
    let artifact = serde_json::from_slice::<ReadReplayArtifact>(&bytes).ok()?;
    if artifact.content_hash != receipt.content_hash {
        return None;
    }
    let revalidated_metadata = source_context
        .fs
        .get_metadata(source_path, Some(&source_context.sandbox))
        .await
        .ok()?;
    let revalidated = source_metadata_token(
        invocation,
        source_context,
        source_path,
        &revalidated_metadata,
    )
    .await?;
    if !receipt.metadata.permits_reuse(&revalidated) {
        return None;
    }
    Some(slice_replayed_read(artifact.output, start_line, line_count))
}

async fn replay_fragmented_read(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    source_path: &PathUri,
    path: &str,
    start_line: usize,
    line_count: usize,
    metadata: &SourceMetadataToken,
) -> (Option<ReadFileSpanOutput>, BTreeSet<usize>) {
    let end_line = start_line.saturating_add(line_count.saturating_sub(1));
    let receipts = invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .find_overlapping_reads(path, start_line, end_line, metadata);
    if receipts.is_empty() {
        return (None, BTreeSet::new());
    }

    let mut outputs = Vec::new();
    let mut content_hash = None;
    for receipt in receipts {
        let Some(bytes) = read_source_replay_artifact(invocation, &receipt.artifact_id).await
        else {
            continue;
        };
        let Ok(artifact) = serde_json::from_slice::<ReadReplayArtifact>(&bytes) else {
            continue;
        };
        if artifact.content_hash != receipt.content_hash
            || content_hash
                .as_ref()
                .is_some_and(|expected| expected != &artifact.content_hash)
        {
            continue;
        }
        content_hash.get_or_insert_with(|| artifact.content_hash.clone());
        outputs.push(artifact.output);
    }
    if outputs.is_empty() {
        return (None, BTreeSet::new());
    }

    let Ok(revalidated_metadata) = source_context
        .fs
        .get_metadata(source_path, Some(&source_context.sandbox))
        .await
    else {
        return (None, BTreeSet::new());
    };
    let Some(revalidated) = source_metadata_token(
        invocation,
        source_context,
        source_path,
        &revalidated_metadata,
    )
    .await
    else {
        return (None, BTreeSet::new());
    };
    if !metadata.permits_reuse(&revalidated) {
        return (None, BTreeSet::new());
    }

    let total_lines = outputs[0].total_lines;
    if outputs
        .iter()
        .any(|output| output.total_lines != total_lines)
    {
        return (None, BTreeSet::new());
    }
    let effective_end = end_line.min(total_lines);
    let mut all_lines = outputs
        .iter()
        .flat_map(|output| output.lines.iter().cloned())
        .filter(|line| line.line_number >= start_line && line.line_number <= effective_end)
        .collect::<Vec<_>>();
    all_lines.sort_by_key(|line| line.line_number);
    all_lines.dedup_by_key(|line| line.line_number);
    let reused_lines = all_lines
        .iter()
        .map(|line| line.line_number)
        .collect::<BTreeSet<_>>();
    let expected_line_count = effective_end.saturating_sub(start_line).saturating_add(1);
    if reused_lines.len() != expected_line_count {
        return (None, reused_lines);
    }

    let mut output = outputs.remove(0);
    output.lines = all_lines;
    output.requested_start_line = start_line;
    output.requested_line_count = line_count;
    output.start_line = output.lines.first().map(|line| line.line_number);
    output.end_line = output.lines.last().map(|line| line.line_number);
    output.bytes_returned = output
        .lines
        .iter()
        .map(|line| line.text.len().saturating_add(1))
        .sum();
    output.truncated = end_line < total_lines && output.end_line != Some(end_line);
    (Some(output), reused_lines)
}

async fn source_metadata_token(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    source_path: &PathUri,
    metadata: &FileMetadata,
) -> Option<SourceMetadataToken> {
    if metadata.is_symlink || !metadata.is_file {
        return None;
    }
    let source_path = source_path.to_abs_path().ok()?;
    let registration = invocation
        .session
        .services
        .git_workspace
        .register_source_freshness_paths([source_path.to_path_buf()])?;
    let watcher_generation = registration.watcher_generation;
    let host_mutation_generation = registration.host_mutation_generation;
    let mutation_revision = invocation.tracker.lock().await.current_mutation_revision();
    let stable_file_identity = stable_file_identity(source_path.as_path())?;
    if !invocation
        .session
        .services
        .git_workspace
        .source_registration_is_current(&registration)
    {
        return None;
    }
    invocation
        .step_context
        .turn
        .source_closure
        .lock()
        .await
        .retain_source_watch(source_path.to_string_lossy().into_owned(), registration);
    Some(SourceMetadataToken {
        size: metadata.size,
        created_at_ms: metadata.created_at_ms,
        modified_at_ms: metadata.modified_at_ms,
        is_symlink: metadata.is_symlink,
        repository_identity: source_context.repo_root_abs.to_string_lossy().into_owned(),
        environment_identity: source_context.environment_id.clone(),
        mutation_revision,
        watcher_generation,
        host_mutation_generation,
        stable_file_identity,
    })
}

#[cfg(unix)]
fn stable_file_identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn stable_file_identity(path: &Path) -> Option<String> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let file = std::fs::File::open(path).ok()?;
    let mut info = MaybeUninit::<FILE_ID_INFO>::zeroed();
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as isize,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let index = info.FileId.Identifier;
    (info.VolumeSerialNumber != 0 || index.iter().any(|byte| *byte != 0))
        .then(|| format!("{}:{index:02x?}", info.VolumeSerialNumber))
}

#[cfg(not(any(unix, windows)))]
fn stable_file_identity(_path: &Path) -> Option<String> {
    None
}

fn slice_replayed_read(
    mut output: ReadFileSpanOutput,
    start_line: usize,
    line_count: usize,
) -> ReadFileSpanOutput {
    if !output.exact_content.is_empty() && start_line >= output.requested_start_line {
        let local_start = start_line - output.requested_start_line + 1;
        if let Ok(mut sliced) = read_file_span_from_bytes(
            output.path.clone(),
            output.exact_content.as_bytes().to_vec(),
            local_start,
            line_count,
        ) {
            let line_offset = output.requested_start_line.saturating_sub(1);
            for line in &mut sliced.lines {
                line.line_number = line.line_number.saturating_add(line_offset);
            }
            for chunk in &mut sliced.chunks {
                chunk.start_line = chunk.start_line.saturating_add(line_offset);
                chunk.end_line = chunk.end_line.saturating_add(line_offset);
                chunk.id = format!(
                    "src:{}:L{}-L{}",
                    &sliced.requested_content_sha256
                        [..sliced.requested_content_sha256.len().min(16)],
                    chunk.start_line,
                    chunk.end_line,
                );
            }
            sliced.source_map_route = output.source_map_route;
            sliced.requested_start_line = start_line;
            sliced.start_line = sliced.lines.first().map(|line| line.line_number);
            sliced.end_line = sliced.lines.last().map(|line| line.line_number);
            sliced.total_lines = output.total_lines;
            sliced.full_file_sha256 = output.full_file_sha256;
            return sliced;
        }
    }
    let requested_end = start_line.saturating_add(line_count.saturating_sub(1));
    output
        .lines
        .retain(|line| line.line_number >= start_line && line.line_number <= requested_end);
    output.requested_start_line = start_line;
    output.requested_line_count = line_count;
    output.start_line = output.lines.first().map(|line| line.line_number);
    output.end_line = output.lines.last().map(|line| line.line_number);
    output.bytes_returned = output
        .lines
        .iter()
        .map(|line| line.text.len().saturating_add(1))
        .sum();
    output.truncated = output
        .end_line
        .is_some_and(|end| end < requested_end.min(output.total_lines));
    output
}

async fn resolve_candidate_owner(
    invocation: &ToolInvocation,
    repository_root: &Path,
    candidate: String,
) {
    let (should_resolve, owner_was_known) = {
        let mut state = invocation.step_context.turn.source_closure.lock().await;
        let already_inside = state.path_is_inside_closure(&candidate);
        let owner_known = state.summary().authoritative_owner.is_some();
        state.add_candidates([candidate.clone()]);
        (!owner_known || !already_inside, owner_known)
    };
    if !should_resolve {
        return;
    }
    let repository_root = repository_root.to_path_buf();
    let manifest_path = repository_root.join("source_owners.toml");
    let resolution = tokio::task::spawn_blocking(move || {
        resolve_owner_candidates(&repository_root, &manifest_path, &[candidate])
    })
    .await;
    if let Ok(resolution) = resolution {
        let owner_is_known = {
            let mut state = invocation.step_context.turn.source_closure.lock().await;
            state.apply_candidate_resolution(resolution);
            state.summary().authoritative_owner.is_some()
        };
        if !owner_was_known && owner_is_known {
            invocation
                .step_context
                .turn
                .turn_timing_state
                .record_source_discovery(SourceDiscoveryTimingEvent::OwnerEstablished);
        }
    }
}

async fn read_loaded_skill_bytes(
    invocation: &ToolInvocation,
    requested_path: &str,
) -> Result<Option<(String, Vec<u8>)>, FunctionCallError> {
    let snapshot = &invocation.step_context.turn.turn_skills.snapshot;
    let outcome = snapshot.outcome();
    let skill = if requested_path.starts_with(codex_core_skills::SKILL_CATALOG_LOCATOR_PREFIX) {
        let Some(skill) = snapshot.resolve_catalog_locator(requested_path) else {
            return Err(FunctionCallError::RespondToModel(format!(
                "unknown loaded skill locator `{requested_path}`"
            )));
        };
        if !outcome.is_skill_enabled(skill) {
            return Err(FunctionCallError::RespondToModel(format!(
                "loaded skill locator `{requested_path}` is disabled"
            )));
        }
        skill
    } else {
        let Ok(requested_path) = AbsolutePathBuf::try_from(requested_path) else {
            return Ok(None);
        };
        let Some(skill) = outcome.skills.iter().find(|skill| {
            outcome.is_skill_enabled(skill) && skill.path_to_skills_md == requested_path
        }) else {
            return Ok(None);
        };
        skill
    };
    let contents = snapshot.read_skill_text(skill).await.map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "unable to read loaded skill `{}`: {err}",
            skill.path_to_skills_md.display()
        ))
    })?;
    let display_path = skill.path_to_skills_md.to_string_lossy().replace('\\', "/");
    Ok(Some((display_path, contents.into_bytes())))
}

async fn record_supporting_source_reads(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    entries: Vec<WorkspaceManifestEntry>,
) -> Result<(), FunctionCallError> {
    await_supporting_read_coordination(
        SOURCE_COORDINATION_MAX_WAIT,
        record_supporting_source_reads_inner(invocation, source_context, entries),
    )
    .await
}

async fn await_supporting_read_coordination<F>(
    max_wait: Duration,
    coordination: F,
) -> Result<(), FunctionCallError>
where
    F: Future<Output = Result<(), FunctionCallError>>,
{
    match tokio::time::timeout(max_wait, coordination).await {
        Ok(result) => result,
        Err(_) => {
            warn!(
                max_wait_ms = max_wait.as_millis(),
                "source read coordination exceeded its time budget; returning confined read output"
            );
            Ok(())
        }
    }
}

async fn record_supporting_source_reads_inner(
    invocation: &ToolInvocation,
    source_context: &LocalSourceContext,
    entries: Vec<WorkspaceManifestEntry>,
) -> Result<(), FunctionCallError> {
    if entries.is_empty() {
        return Ok(());
    }
    let coordinator = invocation.session.services.agent_control.task_coordinator();
    if coordinator.store().is_none()
        && let Err(error) = coordinator
            .initialize_for_workspace_coordination(
                invocation.session.services.state_db.clone(),
                invocation.step_context.turn.config.sqlite_home.clone(),
                invocation
                    .step_context
                    .turn
                    .config
                    .model_provider_id
                    .clone(),
                invocation
                    .session
                    .services
                    .agent_control
                    .session_id()
                    .to_string(),
            )
            .await
    {
        warn!(
            %error,
            "source tool completed without initializing supporting-read coordination"
        );
        return Ok(());
    }
    let binding = coordinator.binding_for_source(&invocation.step_context.turn.session_source);
    let Some(store) = coordinator.store() else {
        warn!("source tool completed without an available supporting-read task store");
        return Ok(());
    };
    let Some(root_session_id) = coordinator.root_session_id() else {
        warn!("source tool completed without a durable root task identity");
        return Ok(());
    };
    let agent_path = invocation
        .step_context
        .turn
        .session_source
        .get_agent_path()
        .map(|path| path.to_string())
        .unwrap_or_else(|| "/root".to_string());
    let (actor_id, kind) = if let Some(binding) = binding.as_ref() {
        (
            format!("attempt:{}", binding.attempt_id),
            WorkspaceActorKind::Typed,
        )
    } else if invocation
        .step_context
        .turn
        .session_source
        .is_non_root_agent()
    {
        (
            format!("legacy:{root_session_id}:{agent_path}"),
            WorkspaceActorKind::Legacy,
        )
    } else {
        (format!("root:{root_session_id}"), WorkspaceActorKind::Root)
    };
    if let Some(binding) = binding.as_ref() {
        match coordinator.heartbeat_typed_actor_binding(binding).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(FunctionCallError::RespondToModel(
                    "read_file_span: the bound typed assignment attempt is no longer active"
                        .to_string(),
                ));
            }
            Err(error) => {
                warn!(
                    %error,
                    "source tool completed without persisting the typed reader heartbeat"
                );
                return Ok(());
            }
        }
    }
    if kind != WorkspaceActorKind::Typed
        && let Err(error) = store
            .register_workspace_actor(
                source_context.repo_root_abs.as_path(),
                WorkspaceActorRegistration {
                    root_session_id,
                    actor_id: actor_id.clone(),
                    kind,
                    assignment_id: None,
                    attempt_id: None,
                    strategy: WorkspaceStrategy::Shared,
                },
            )
            .await
    {
        warn!(
            %error,
            "source tool completed without registering its durable reader identity"
        );
        return Ok(());
    }
    if let Err(error) = store
        .record_supporting_read_entries(source_context.repo_root_abs.as_path(), actor_id, entries)
        .await
    {
        warn!(
            %error,
            "source tool completed without persisting its supporting-read manifest"
        );
    } else if let Some(binding) = binding {
        coordinator.record_first_meaningful_progress_once(
            binding.attempt_id,
            codex_agent_task_store::ObservationKind::Reading,
            &invocation.step_context.turn.session_telemetry,
        );
    }
    Ok(())
}

fn reject_unadvertised_environment_id(
    tool_name: &str,
    options: SourceToolOptions,
    environment_id: Option<&str>,
) -> Result<(), FunctionCallError> {
    if !options.include_environment_id && environment_id.is_some() {
        return Err(FunctionCallError::RespondToModel(format!(
            "failed to parse {tool_name} arguments: unknown field `environment_id`"
        )));
    }
    Ok(())
}

struct LocalSourceContext {
    fs: Arc<dyn ExecutorFileSystem>,
    sandbox: FileSystemSandboxContext,
    repo_root: PathUri,
    repo_root_abs: AbsolutePathBuf,
    is_git_repository: bool,
    environment_id: String,
}

async fn local_source_context(
    invocation: &ToolInvocation,
    environment_id: Option<&str>,
) -> Result<LocalSourceContext, FunctionCallError> {
    let environment =
        resolve_tool_environment(&invocation.step_context.environments, environment_id)?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "source tools require a selected local environment".to_string(),
                )
            })?;
    if environment.environment.is_remote() {
        return Err(FunctionCallError::RespondToModel(
            "source tools currently support local environments only".to_string(),
        ));
    }
    let sandbox = invocation
        .step_context
        .turn
        .file_system_sandbox_context(/*additional_permissions*/ None, environment.cwd());
    let fs = environment.environment.get_filesystem();
    let cwd = fs
        .canonicalize(environment.cwd(), Some(&sandbox))
        .await
        .map_err(|err| source_fs_error("canonicalize", environment.cwd(), err))?;
    let cwd_metadata = fs
        .get_metadata(&cwd, Some(&sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", &cwd, err))?;
    if !cwd_metadata.is_directory {
        return Err(FunctionCallError::RespondToModel(format!(
            "source tool cwd `{}` is not a directory",
            cwd.inferred_native_path_string()
        )));
    }
    let (repo_root, is_git_repository) = find_repo_root(fs.as_ref(), &sandbox, &cwd).await?;
    let repo_root_abs = repo_root.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source repo root is not host-native: {err}"))
    })?;
    Ok(LocalSourceContext {
        fs,
        sandbox,
        repo_root,
        repo_root_abs,
        is_git_repository,
        environment_id: environment.environment_id.clone(),
    })
}

async fn find_repo_root(
    fs: &dyn ExecutorFileSystem,
    sandbox: &FileSystemSandboxContext,
    cwd: &PathUri,
) -> Result<(PathUri, bool), FunctionCallError> {
    for ancestor in cwd.ancestors() {
        let dot_git = ancestor.join(".git").map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to resolve repository marker below `{}`: {err}",
                ancestor.inferred_native_path_string()
            ))
        })?;
        match fs.get_metadata(&dot_git, Some(sandbox)).await {
            Ok(metadata) if metadata.is_directory || metadata.is_file => {
                return Ok((ancestor, true));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(source_fs_error("inspect", &dot_git, err)),
        }
    }
    Ok((cwd.clone(), false))
}

async fn resolve_search_roots(
    context: &LocalSourceContext,
    roots: &[PathBuf],
) -> Result<Vec<PathUri>, FunctionCallError> {
    validate_search_root_count(roots)?;
    let mut roots = if roots.is_empty() {
        vec![context.repo_root.clone()]
    } else {
        let mut resolved = Vec::with_capacity(roots.len());
        for root in roots {
            resolved.push(
                resolve_confined_path(context, &root.to_string_lossy(), "source root").await?,
            );
        }
        resolved
    };
    roots.sort_by(|left, right| {
        left.ancestors()
            .count()
            .cmp(&right.ancestors().count())
            .then_with(|| left.to_string().cmp(&right.to_string()))
    });
    roots.dedup();
    let mut deduped = Vec::<PathUri>::new();
    for root in roots {
        if deduped.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        deduped.push(root);
    }
    Ok(deduped)
}

fn validate_search_root_count(roots: &[PathBuf]) -> Result<(), FunctionCallError> {
    if roots.len() > SOURCE_SEARCH_MAX_ROOTS {
        return Err(FunctionCallError::RespondToModel(format!(
            "too many source roots ({} provided, max {})",
            roots.len(),
            SOURCE_SEARCH_MAX_ROOTS
        )));
    }
    Ok(())
}

async fn resolve_confined_path(
    context: &LocalSourceContext,
    path: &str,
    label: &str,
) -> Result<PathUri, FunctionCallError> {
    let candidate = context.repo_root.join(path).map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve {label} `{path}`: {err}"))
    })?;
    let canonical = context
        .fs
        .canonicalize(&candidate, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("canonicalize", &candidate, err))?;
    if !canonical.starts_with(&context.repo_root) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{label} `{path}` resolves outside repository root `{}`",
            context.repo_root.inferred_native_path_string()
        )));
    }
    Ok(canonical)
}

async fn load_repository_exclude_rules(
    context: &LocalSourceContext,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<(), FunctionCallError> {
    if !context.is_git_repository {
        return Ok(());
    }
    let dot_git = context.repo_root.join(".git").map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "unable to resolve repository ignore metadata: {err}"
        ))
    })?;
    let git_common_directory = match context
        .fs
        .get_metadata(&dot_git, Some(&context.sandbox))
        .await
    {
        Ok(metadata) if metadata.is_directory => Some(dot_git),
        Ok(metadata) if metadata.is_file => resolve_git_common_directory(context, &dot_git).await?,
        Ok(_) => None,
        Err(err) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unable to inspect repository ignore metadata `{dot_git}`: {err}"
            )));
        }
    };
    let Some(git_common_directory) = git_common_directory else {
        return Ok(());
    };
    let Some(exclude_path) = git_common_directory.join("info/exclude").ok() else {
        return Ok(());
    };
    let Some(contents) = read_optional_ignore_text(context, &exclude_path).await? else {
        return Ok(());
    };
    let source_path = exclude_path.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "repository exclude path is not host-native: {err}"
        ))
    })?;
    ignore_matcher.set_repository_exclude(
        context.repo_root_abs.as_path(),
        source_path.as_path(),
        &contents,
    );
    Ok(())
}

async fn resolve_git_common_directory(
    context: &LocalSourceContext,
    dot_git: &PathUri,
) -> Result<Option<PathUri>, FunctionCallError> {
    let Some(contents) = read_optional_ignore_text(context, dot_git).await? else {
        return Ok(None);
    };
    let Some(git_dir_target) = contents.strip_prefix("gitdir:").map(str::trim) else {
        return Ok(None);
    };
    if git_dir_target.is_empty() {
        return Ok(None);
    }
    let git_directory = context.repo_root.join(git_dir_target).map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve Git directory: {err}"))
    })?;
    let git_directory = context
        .fs
        .canonicalize(&git_directory, Some(&context.sandbox))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to canonicalize Git directory: {err}"
            ))
        })?;
    let common_dir_path = git_directory.join("commondir").map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve Git common directory: {err}"))
    })?;
    let Some(common_dir) = read_optional_ignore_text(context, &common_dir_path).await? else {
        return Ok(Some(git_directory));
    };
    let common_dir = common_dir.trim();
    if common_dir.is_empty() {
        return Ok(Some(git_directory));
    }
    let common_directory = git_directory.join(common_dir).map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve Git common directory: {err}"))
    })?;
    let common_directory = context
        .fs
        .canonicalize(&common_directory, Some(&context.sandbox))
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "unable to canonicalize Git common directory: {err}"
            ))
        })?;
    Ok(Some(common_directory))
}

async fn read_optional_ignore_text(
    context: &LocalSourceContext,
    path: &PathUri,
) -> Result<Option<String>, FunctionCallError> {
    let read_result = if path.starts_with(&context.repo_root) {
        context
            .fs
            .read_file_bounded_confined(
                path,
                &context.repo_root,
                SOURCE_SEARCH_MAX_FILE_BYTES,
                Some(&context.sandbox),
            )
            .await
    } else {
        context
            .fs
            .read_file_bounded(path, SOURCE_SEARCH_MAX_FILE_BYTES, Some(&context.sandbox))
            .await
    };
    let bytes = match read_result {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(FunctionCallError::RespondToModel(format!(
                "unable to read optional source ignore file `{path}`: {err}"
            )));
        }
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    String::from_utf8(bytes).map(Some).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "optional source ignore file `{path}` is not UTF-8: {err}"
        ))
    })
}

async fn load_directory_ignore_rules(
    context: &LocalSourceContext,
    directory: &PathUri,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<(), FunctionCallError> {
    let directory_path = directory.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "source ignore directory is not host-native: {err}"
        ))
    })?;
    if ignore_matcher.has_directory_rules(directory_path.as_path()) {
        return Ok(());
    }
    let ignore_path = directory.join(".ignore").map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve .ignore path: {err}"))
    })?;
    let git_ignore_path = directory.join(".gitignore").map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to resolve .gitignore path: {err}"))
    })?;
    let ignore_contents = read_optional_ignore_text(context, &ignore_path).await?;
    let git_ignore_contents = read_optional_ignore_text(context, &git_ignore_path).await?;
    ignore_matcher.add_directory_rules(
        directory_path.as_path(),
        ignore_contents.as_deref(),
        git_ignore_contents.as_deref(),
    );
    Ok(())
}

async fn load_ignore_rules_through(
    context: &LocalSourceContext,
    directory: &PathUri,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<(), FunctionCallError> {
    let mut ancestors = directory
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(&context.repo_root))
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        load_directory_ignore_rules(context, &ancestor, ignore_matcher).await?;
    }
    Ok(())
}

fn source_path_is_ignored(
    path: &PathUri,
    is_directory: bool,
    ignore_matcher: &SourceIgnoreMatcher,
) -> Result<bool, FunctionCallError> {
    let path = path.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source ignore path is not host-native: {err}"))
    })?;
    Ok(ignore_matcher.is_ignored(path.as_path(), is_directory))
}

const SOURCE_SEARCH_REPLAY_CONTRACT_VERSION: &str = "source_search_replay_v1";

fn update_scope_metadata(hasher: &mut Sha256, path: &str, metadata: &FileMetadata) -> bool {
    if metadata.created_at_ms <= 0 || metadata.modified_at_ms <= 0 {
        return false;
    }
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(metadata.size.to_le_bytes());
    hasher.update(metadata.created_at_ms.to_le_bytes());
    hasher.update(metadata.modified_at_ms.to_le_bytes());
    hasher.update([
        u8::from(metadata.is_file),
        u8::from(metadata.is_directory),
        u8::from(metadata.is_symlink),
    ]);
    true
}

async fn hash_ignore_dependency(
    context: &LocalSourceContext,
    path: &PathUri,
    hasher: &mut Sha256,
) -> Result<bool, FunctionCallError> {
    let metadata = match context.fs.get_metadata(path, Some(&context.sandbox)).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(source_fs_error("inspect search dependency", path, err)),
    };
    if !metadata.is_file || metadata.is_symlink {
        return Ok(false);
    }
    if !update_scope_metadata(hasher, &path.to_string(), &metadata) {
        return Ok(false);
    }
    let Some(bytes) = read_source_file_stably(context, path, &metadata).await? else {
        return Ok(false);
    };
    hasher.update(Sha256::digest(bytes));
    Ok(true)
}

/// Build a complete metadata/topology identity for the bounded roots. The walk
/// deliberately visits excluded subtrees too: if any path under a root cannot
/// be accounted for within the hard bounds, replay is disabled and the normal
/// content search runs.
async fn complete_search_scope_revision(
    context: &LocalSourceContext,
    roots: &[PathUri],
    options: &SourceSearchOptions,
) -> Result<Option<String>, FunctionCallError> {
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_SEARCH_REPLAY_CONTRACT_VERSION.as_bytes());
    hasher.update(context.repo_root_abs.as_path().to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(context.environment_id.as_bytes());
    hasher.update(
        serde_json::to_vec(&json!({
            "case_sensitive": options.case_sensitive,
            "include_generated": options.include_generated,
            "include_vendor": options.include_vendor,
            "include_locks": options.include_locks,
            "context_lines": options.context_lines,
            "max_matches": options.max_matches,
        }))
        .unwrap_or_default(),
    );

    let mut ignore_dependencies = Vec::new();
    let mut seen_ignore_dependencies = BTreeSet::new();
    for root in roots {
        let directory = match context.fs.get_metadata(root, Some(&context.sandbox)).await {
            Ok(metadata) if metadata.is_directory => root.clone(),
            Ok(_) => root.parent().unwrap_or_else(|| context.repo_root.clone()),
            Err(err) => return Err(source_fs_error("inspect search root", root, err)),
        };
        for ancestor in directory
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(&context.repo_root))
        {
            for name in [".gitignore", ".ignore"] {
                let dependency = ancestor.join(name).map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "unable to resolve search dependency: {err}"
                    ))
                })?;
                if seen_ignore_dependencies.insert(dependency.to_string()) {
                    ignore_dependencies.push(dependency);
                }
            }
        }
    }
    if context.is_git_repository {
        let git_exclude = context
            .repo_root
            .join(".git/info/exclude")
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        if seen_ignore_dependencies.insert(git_exclude.to_string()) {
            ignore_dependencies.push(git_exclude);
        }
    }
    for dependency in ignore_dependencies {
        if !hash_ignore_dependency(context, &dependency, &mut hasher).await? {
            return Ok(None);
        }
    }

    let mut queue = VecDeque::from_iter(roots.iter().cloned());
    let mut visited = BTreeSet::new();
    let mut walked_entries = 0usize;
    let mut walked_directories = 0usize;
    while let Some(path) = queue.pop_front() {
        let path_key = path.to_string();
        if !visited.insert(path_key.clone()) {
            continue;
        }
        let metadata = match context.fs.get_metadata(&path, Some(&context.sandbox)).await {
            Ok(metadata) => metadata,
            Err(_) => return Ok(None),
        };
        if !update_scope_metadata(&mut hasher, &path_key, &metadata) || metadata.is_symlink {
            return Ok(None);
        }
        if !metadata.is_directory {
            if !metadata.is_file {
                return Ok(None);
            }
            let Some(bytes) = read_source_file_stably(context, &path, &metadata).await? else {
                return Ok(None);
            };
            // Metadata/topology detects ordinary changes; the content digest
            // closes same-size/same-timestamp replacement gaps for an exact
            // search result identity.
            hasher.update(Sha256::digest(bytes));
            continue;
        }
        walked_directories = walked_directories.saturating_add(1);
        if walked_directories > SOURCE_SEARCH_MAX_WALK_DIRECTORIES {
            return Ok(None);
        }
        let remaining = SOURCE_SEARCH_MAX_WALK_ENTRIES.saturating_sub(walked_entries);
        if remaining == 0 {
            return Ok(None);
        }
        let outcome = match context
            .fs
            .read_directory_bounded(&path, remaining, Some(&context.sandbox))
            .await
        {
            Ok(outcome) => outcome,
            Err(_) => return Ok(None),
        };
        if outcome.limit_reached
            || outcome.entries_examined > remaining
            || outcome.entries.len() > outcome.entries_examined
        {
            return Ok(None);
        }
        walked_entries = walked_entries.saturating_add(outcome.entries_examined);
        let mut entries = outcome.entries;
        entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));
        for entry in entries {
            let child = match path.join(&entry.file_name) {
                Ok(child) => child,
                Err(_) => return Ok(None),
            };
            queue.push_back(child);
        }
    }
    Ok(Some(format!("{:x}", hasher.finalize())))
}

fn exact_search_key(
    options: &SourceSearchOptions,
    roots: &[String],
    repository_root: &Path,
    environment_id: &str,
) -> String {
    let value = json!({
        "contract": SOURCE_SEARCH_REPLAY_CONTRACT_VERSION,
        "repository": repository_root.to_string_lossy(),
        "environment_id": environment_id,
        "query": options.query,
        "roots": roots,
        "max_matches": options.max_matches,
        "context_lines": options.context_lines,
        "case_sensitive": options.case_sensitive,
        "include_generated": options.include_generated,
        "include_vendor": options.include_vendor,
        "include_locks": options.include_locks,
        "hydrate_selected_span": options.hydrate_selected_span,
    });
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).unwrap_or_default())
    )
}

async fn scan_source_root(
    context: &LocalSourceContext,
    root: &PathUri,
    options: &SourceSearchOptions,
    ignore_matcher: &SourceIgnoreMatcher,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
) -> Result<(), FunctionCallError> {
    let metadata = context
        .fs
        .get_metadata(root, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", root, err))?;
    if metadata.is_file {
        return add_source_file(context, root, accumulator, observed_entries, false).await;
    }
    if !metadata.is_directory {
        return Err(FunctionCallError::RespondToModel(format!(
            "source root `{}` is neither a file nor a directory",
            root.inferred_native_path_string()
        )));
    }
    load_ignore_rules_through(context, root, ignore_matcher).await?;
    if root != &context.repo_root && source_path_is_ignored(root, true, ignore_matcher)? {
        return Ok(());
    }

    let mut queue = VecDeque::from([(root.clone(), 0usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        if accumulator.should_stop() {
            break;
        }
        if !accumulator.reserve_walk_directory(SOURCE_SEARCH_MAX_WALK_DIRECTORIES) {
            break;
        }
        load_directory_ignore_rules(context, &directory, ignore_matcher).await?;
        let remaining_entries = accumulator.remaining_walk_entries(SOURCE_SEARCH_MAX_WALK_ENTRIES);
        if remaining_entries == 0 {
            accumulator.mark_walk_limit();
            return Ok(());
        }
        let entries_result = context
            .fs
            .read_directory_bounded(&directory, remaining_entries, Some(&context.sandbox))
            .await;
        let outcome = if depth == 0 {
            entries_result.map_err(|err| source_fs_error("read directory", &directory, err))?
        } else {
            let Some(outcome) = recover_scan_result(entries_result, accumulator) else {
                continue;
            };
            outcome
        };
        if outcome.entries_examined > remaining_entries
            || outcome.entries.len() > outcome.entries_examined
        {
            return Err(FunctionCallError::RespondToModel(
                "bounded directory read returned an invalid entry count".to_string(),
            ));
        }
        accumulator.record_walk_entries(outcome.entries_examined, SOURCE_SEARCH_MAX_WALK_ENTRIES);
        let limit_reached = outcome.limit_reached;
        let mut entries = outcome.entries;
        entries.sort_by(|left, right| left.file_name.cmp(&right.file_name));

        for entry in entries {
            if accumulator.should_stop() {
                break;
            }
            let Some(child) = recover_scan_result(directory.join(&entry.file_name), accumulator)
            else {
                continue;
            };
            if entry.is_directory {
                let Some(child_metadata) = recover_scan_result(
                    context
                        .fs
                        .get_metadata(&child, Some(&context.sandbox))
                        .await,
                    accumulator,
                ) else {
                    continue;
                };
                let Some(relative) =
                    recover_scan_result(relative_source_path(context, &child), accumulator)
                else {
                    continue;
                };
                if !child_metadata.is_directory
                    || child_metadata.is_symlink
                    || !should_descend_source_path(
                        Path::new(&relative),
                        options.include_generated,
                        options.include_vendor,
                    )
                    || source_path_is_ignored(&child, true, ignore_matcher)?
                {
                    accumulator.record_ignored_entries(1);
                    continue;
                }
                if depth >= SOURCE_SEARCH_MAX_WALK_DEPTH {
                    accumulator.mark_walk_limit();
                    continue;
                }
                queue.push_back((child, depth.saturating_add(1)));
                continue;
            }
            if !entry.is_file {
                accumulator.record_ignored_entries(1);
                continue;
            }
            let Some(relative) =
                recover_scan_result(relative_source_path(context, &child), accumulator)
            else {
                continue;
            };
            if !should_scan_source_file(
                Path::new(&relative),
                options.include_generated,
                options.include_vendor,
                options.include_locks,
            ) || source_path_is_ignored(&child, false, ignore_matcher)?
            {
                accumulator.record_ignored_entries(1);
                continue;
            }
            let Some(canonical) = recover_scan_result(
                context
                    .fs
                    .canonicalize(&child, Some(&context.sandbox))
                    .await,
                accumulator,
            ) else {
                continue;
            };
            if !canonical.starts_with(&context.repo_root) {
                accumulator.mark_filesystem_error();
                continue;
            }
            let _ = recover_scan_result(
                add_source_file(context, &canonical, accumulator, observed_entries, true).await,
                accumulator,
            );
        }
        if limit_reached {
            accumulator.mark_walk_limit();
            return Ok(());
        }
    }
    Ok(())
}

async fn scan_source_roots(
    context: &LocalSourceContext,
    roots: &[PathUri],
    options: &SourceSearchOptions,
    ignore_matcher: &SourceIgnoreMatcher,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
    recover_root_failures: bool,
) -> Result<(), FunctionCallError> {
    for root in roots {
        if accumulator.should_stop() {
            break;
        }
        let result = scan_source_root(
            context,
            root,
            options,
            ignore_matcher,
            accumulator,
            observed_entries,
        )
        .await;
        if recover_root_failures {
            let _ = recover_scan_result(result, accumulator);
        } else {
            result?;
        }
    }
    Ok(())
}

async fn add_source_file(
    context: &LocalSourceContext,
    path: &PathUri,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
    already_filtered: bool,
) -> Result<(), FunctionCallError> {
    let started = Instant::now();
    let result = add_source_file_inner(
        context,
        path,
        accumulator,
        observed_entries,
        already_filtered,
    )
    .await;
    accumulator.record_file_scan_match_duration(started.elapsed());
    result
}

async fn add_source_file_inner(
    context: &LocalSourceContext,
    path: &PathUri,
    accumulator: &mut SourceSearchAccumulator,
    observed_entries: &mut BTreeMap<String, WorkspaceManifestEntry>,
    already_filtered: bool,
) -> Result<(), FunctionCallError> {
    let metadata = context
        .fs
        .get_metadata(path, Some(&context.sandbox))
        .await
        .map_err(|err| source_fs_error("inspect", path, err))?;
    if !metadata.is_file {
        return Err(FunctionCallError::RespondToModel(format!(
            "source path `{}` is not a file",
            path.inferred_native_path_string()
        )));
    }
    let relative = relative_source_path(context, path)?;
    let file_len = usize::try_from(metadata.size).unwrap_or(usize::MAX);
    let should_scan = if already_filtered {
        accumulator.consider_walked_file(file_len)
    } else {
        accumulator.consider_file(Path::new(&relative), file_len)
    };
    if !should_scan {
        return Ok(());
    }
    match read_source_file_stably(context, path, &metadata).await? {
        Some(bytes) => {
            observed_entries.insert(
                relative.clone(),
                manifest_entry_from_bytes(relative.clone(), &bytes),
            );
            accumulator.add_file_bytes(Path::new(&relative), bytes);
        }
        None => accumulator.mark_file_changed_during_read(),
    }
    Ok(())
}

fn manifest_entry_from_bytes(path: String, bytes: &[u8]) -> WorkspaceManifestEntry {
    WorkspaceManifestEntry {
        path,
        content_hash: Some(format!("{:x}", Sha256::digest(bytes))),
        existed: true,
    }
}

fn hash_observed_source_span(
    bytes: &[u8],
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Option<String> {
    let start_line = start_line?;
    let end_line = end_line?;
    let text = std::str::from_utf8(bytes).ok()?;
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    if start_line == 0 || start_line > lines.len() || end_line < start_line {
        return None;
    }
    let selected = lines
        .get(start_line - 1..end_line.min(lines.len()))?
        .concat();
    Some(format!("{:x}", Sha256::digest(selected.as_bytes())))
}

async fn read_source_file_stably(
    context: &LocalSourceContext,
    path: &PathUri,
    metadata_before: &FileMetadata,
) -> Result<Option<Vec<u8>>, FunctionCallError> {
    let expected_len = usize::try_from(metadata_before.size).unwrap_or(usize::MAX);
    let bytes = match context
        .fs
        .read_file_bounded_confined(
            path,
            &context.repo_root,
            SOURCE_SEARCH_MAX_FILE_BYTES,
            Some(&context.sandbox),
        )
        .await
    {
        Ok(bytes) => bytes,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => return Err(source_fs_error("read", path, err)),
    };
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    #[cfg(test)]
    test_observation::record_successful_content_read();
    let metadata_after = match context.fs.get_metadata(path, Some(&context.sandbox)).await {
        Ok(metadata) => metadata,
        Err(err) if is_changed_file_race_error(err.kind()) => return Ok(None),
        Err(err) => return Err(source_fs_error("re-inspect", path, err)),
    };
    if bytes.len() != expected_len || source_metadata_changed(metadata_before, &metadata_after) {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn source_metadata_changed(before: &FileMetadata, after: &FileMetadata) -> bool {
    before.size != after.size
        || before.created_at_ms != after.created_at_ms
        || before.modified_at_ms != after.modified_at_ms
        || before.is_file != after.is_file
        || before.is_directory != after.is_directory
        || before.is_symlink != after.is_symlink
}

fn is_changed_file_race_error(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::NotFound | ErrorKind::InvalidInput)
}

fn recover_scan_result<T, E>(
    result: Result<T, E>,
    accumulator: &mut SourceSearchAccumulator,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(_) => {
            accumulator.mark_filesystem_error();
            None
        }
    }
}

fn relative_source_path(
    context: &LocalSourceContext,
    path: &PathUri,
) -> Result<String, FunctionCallError> {
    let path = path.to_abs_path().map_err(|err| {
        FunctionCallError::RespondToModel(format!("source path is not host-native: {err}"))
    })?;
    let relative = path.strip_prefix(&context.repo_root_abs).map_err(|_| {
        FunctionCallError::RespondToModel(format!(
            "source path `{}` is outside repository root `{}`",
            path.display(),
            context.repo_root_abs.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Ok(".".to_string());
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn source_fs_error(action: &str, path: &PathUri, err: std::io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "unable to {action} source path `{}`: {err}",
        path.inferred_native_path_string()
    ))
}

fn render_search_output(output: &SourceSearchOutput) -> String {
    let mut rendered = BoundedSourceOutput::new();
    let _ = rendered.push_line("Source search evidence:".to_string());
    let _ = rendered.push_line(format!("query: {}", output.query));
    let coverage_line_index = rendered.line_count();
    let _ = rendered.push_line(render_search_coverage(output, output.truncated));
    if let Some(reason) = output.truncated_reason {
        let _ = rendered.push_line(format!("truncated_reason: {reason:?}"));
    }
    if let Some(note) = &output.coverage_note {
        let _ = rendered.push_line(format!("coverage_note: {note}"));
    }
    let first_match_micros = output
        .diagnostics
        .first_match_micros
        .map_or_else(|| "none".to_string(), |duration| duration.to_string());
    let _ = rendered.push_line(format!(
        "diagnostics: total_us={} first_match_us={first_match_micros} traversal_us={} file_scan_match_us={} projection_us={}",
        output.diagnostics.total_micros,
        output.diagnostics.traversal_micros,
        output.diagnostics.file_scan_match_micros,
        output.diagnostics.projection_micros,
    ));
    let render_reason_index = rendered.line_count();

    'matches: for source_match in &output.matches {
        let mut metadata = vec![
            String::new(),
            format!("match_id: {}", source_match.id),
            format!("file_id: {}", source_match.file_id),
            format!(
                "citation: {}:{}-{} (match line {})",
                source_match.path,
                source_match.start_line,
                source_match.end_line,
                source_match.line_number
            ),
            format!("source_revision: {}", source_match.source_revision),
            format!("matched_content: {}", source_match.matched_content),
            format!("context_complete: {}", source_match.context_complete),
        ];
        if let Some(route) = &source_match.source_map_route {
            metadata.push(format!("source_route: {route}"));
        }
        if !rendered.push_lines(metadata) {
            break;
        }
        for line in &source_match.lines {
            if !rendered.push_source_line(line.line_number, &line.text, line.text_truncated) {
                break 'matches;
            }
        }
    }

    let _ = rendered.push_line(format!("hydration_status: {:?}", output.hydration_status));
    if let Some(hydrated) = &output.hydrated_span {
        let observation = &hydrated.observation;
        let _ = rendered.push_line(String::new());
        let _ = rendered.push_line(format!(
            "hydrated_citation: {}:{}-{}",
            observation.path,
            observation
                .start_line
                .unwrap_or(observation.requested_start_line),
            observation
                .end_line
                .unwrap_or(observation.requested_start_line)
        ));
        let _ = rendered.push_line(format!("observed_content_hash: {}", hydrated.content_hash));
        for line in &observation.lines {
            if !rendered.push_source_line(line.line_number, &line.text, line.text_truncated) {
                break;
            }
        }
    }

    rendered.finish(
        coverage_line_index,
        render_search_coverage(output, true),
        render_reason_index,
    )
}

fn render_search_coverage(output: &SourceSearchOutput, truncated: bool) -> String {
    format!(
        "coverage: complete={} index_complete={} context_complete={} walked={} ignored={} files={} skipped_too_large={} skipped_non_utf8={} changed_during_read={} filesystem_errors={} bytes={} total_matches={} indexed={} returned={} omitted_contexts={} result_cap_reached={} truncated={truncated}",
        output.coverage_complete,
        output.coverage.index_complete,
        output.coverage.context_complete,
        output.coverage.walked_entries,
        output.coverage.ignored_entries,
        output.coverage.files_scanned,
        output.coverage.files_skipped_too_large,
        output.coverage.files_skipped_non_utf8,
        output.coverage.files_changed_during_read,
        output.coverage.filesystem_errors,
        output.coverage.bytes_scanned,
        output.coverage.total_matches,
        output.coverage.indexed_matches,
        output.coverage.matches_returned,
        output.coverage.omitted_contexts,
        output.coverage.result_cap_reached,
    )
}

fn render_read_output(output: &ReadFileSpanOutput) -> String {
    let citation = match (output.start_line, output.end_line) {
        (Some(start), Some(end)) => format!("{}:{start}-{end}", output.path),
        _ => format!("{}:<empty>", output.path),
    };
    let mut rendered = BoundedSourceOutput::new();
    let _ = rendered.push_line("Source file evidence:".to_string());
    let _ = rendered.push_line(format!("citation: {citation}"));
    let summary_line_index = rendered.line_count();
    let _ = rendered.push_line(render_read_summary(output, output.truncated));
    let render_reason_index = rendered.line_count();
    if let Some(route) = &output.source_map_route
        && !rendered.push_line(format!("source_route: {route}"))
    {
        return rendered.finish(
            summary_line_index,
            render_read_summary(output, true),
            render_reason_index,
        );
    }
    for line in &output.lines {
        if !rendered.push_source_line(line.line_number, &line.text, line.text_truncated) {
            break;
        }
    }

    rendered.finish(
        summary_line_index,
        render_read_summary(output, true),
        render_reason_index,
    )
}

fn render_read_summary(output: &ReadFileSpanOutput, truncated: bool) -> String {
    format!(
        "total_lines: {} bytes_returned: {} truncated: {truncated}",
        output.total_lines, output.bytes_returned,
    )
}

struct BoundedSourceOutput {
    lines: Vec<String>,
    rendered_bytes: usize,
    content_limit: usize,
    render_truncated: bool,
}

impl BoundedSourceOutput {
    fn new() -> Self {
        let marker = source_output_truncation_marker();
        let reserved_bytes = "\nrender_truncated_reason: MaxRenderedBytes"
            .len()
            .saturating_add(1)
            .saturating_add(marker.len());
        Self {
            lines: Vec::new(),
            rendered_bytes: 0,
            content_limit: SOURCE_TOOL_MAX_RENDERED_BYTES.saturating_sub(reserved_bytes),
            render_truncated: false,
        }
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn push_line(&mut self, line: String) -> bool {
        let separator_bytes = usize::from(!self.lines.is_empty());
        let additional_bytes = separator_bytes.saturating_add(line.len());
        if self.rendered_bytes.saturating_add(additional_bytes) > self.content_limit {
            self.render_truncated = true;
            return false;
        }
        self.rendered_bytes = self.rendered_bytes.saturating_add(additional_bytes);
        self.lines.push(line);
        true
    }

    fn push_lines(&mut self, lines: Vec<String>) -> bool {
        let additional_bytes = lines
            .iter()
            .enumerate()
            .fold(0usize, |total, (index, line)| {
                total
                    .saturating_add(usize::from(!self.lines.is_empty() || index > 0))
                    .saturating_add(line.len())
            });
        if self.rendered_bytes.saturating_add(additional_bytes) > self.content_limit {
            self.render_truncated = true;
            return false;
        }
        self.rendered_bytes = self.rendered_bytes.saturating_add(additional_bytes);
        self.lines.extend(lines);
        true
    }

    fn push_source_line(&mut self, line_number: usize, text: &str, text_truncated: bool) -> bool {
        let prefix = format!("{line_number:>6} | ");
        let suffix = if text_truncated {
            " [line truncated]"
        } else {
            ""
        };
        let separator_bytes = usize::from(!self.lines.is_empty());
        let full_bytes = separator_bytes
            .saturating_add(prefix.len())
            .saturating_add(text.len())
            .saturating_add(suffix.len());
        if self.rendered_bytes.saturating_add(full_bytes) <= self.content_limit {
            let mut line = String::with_capacity(prefix.len() + text.len() + suffix.len());
            line.push_str(&prefix);
            line.push_str(text);
            line.push_str(suffix);
            return self.push_line(line);
        }

        self.render_truncated = true;
        let remaining = self
            .content_limit
            .saturating_sub(self.rendered_bytes)
            .saturating_sub(separator_bytes);
        let truncated_suffix = " [line truncated]";
        let fixed_bytes = prefix.len().saturating_add(truncated_suffix.len());
        if remaining < fixed_bytes {
            return false;
        }
        let mut text_end = remaining.saturating_sub(fixed_bytes).min(text.len());
        while text_end > 0 && !text.is_char_boundary(text_end) {
            text_end -= 1;
        }
        let mut line = String::with_capacity(prefix.len() + text_end + truncated_suffix.len());
        line.push_str(&prefix);
        line.push_str(&text[..text_end]);
        line.push_str(truncated_suffix);
        let _ = self.push_line(line);
        false
    }

    fn finish(
        mut self,
        truncated_line_index: usize,
        truncated_line: String,
        render_reason_index: usize,
    ) -> String {
        if !self.render_truncated {
            return self.lines.join("\n");
        }
        if let Some(line) = self.lines.get_mut(truncated_line_index) {
            *line = truncated_line;
        }
        self.lines.insert(
            render_reason_index.min(self.lines.len()),
            "render_truncated_reason: MaxRenderedBytes".to_string(),
        );
        let mut rendered = self.lines.join("\n");
        rendered.push('\n');
        rendered.push_str(&source_output_truncation_marker());
        debug_assert!(rendered.len() <= SOURCE_TOOL_MAX_RENDERED_BYTES);
        rendered
    }
}

fn source_output_truncation_marker() -> String {
    format!("[source tool output truncated at {SOURCE_TOOL_MAX_RENDERED_BYTES} bytes]")
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
