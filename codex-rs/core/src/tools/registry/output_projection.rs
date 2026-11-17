//! Model-visible output selection, authenticated recovery and bounded delivery.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalByteRange;
use codex_tools::CanonicalJsonPointer;
use codex_tools::CanonicalToolResult;
use codex_tools::CanonicalToolResultKind;
use codex_tools::ToolName;
use codex_tools::ToolOutputDiagnosticClass;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputProjectionFragment;
use codex_tools::ToolOutputProjectionFragmentKind;
use codex_tools::ToolOutputProjectionJsonPointer;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolOutputProjectionRange;
use codex_tools::ToolProjectionInclusion;
use codex_tools::ToolProjectionSection;
use codex_tools::ToolProjectionV1;
use codex_utils_output_truncation::OutputDiagnosticClass;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::resolve_projected_output_limits;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use serde_json::Value;

use crate::session::turn_execution::PendingOwnerDrainedContinuation;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::attach_canonical_output_artifact;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_ceiling_and_reuse;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::flat_tool_name;
use crate::tools::handlers::ParsedFunctionArguments;
use crate::turn_timing::TurnTimingState;

use super::AnyToolResult;
use super::UnavailableModelProjectionOutput;
use super::authorized_tool_invocation_sha256;
use super::precomputed_projection_source_dependencies;
use super::projection_is_provider_visible;

const MIN_PROJECTION_ENVELOPE_TOKENS: usize = 64;

pub(crate) struct ModelToolProjection {
    original_response: ResponseInputItem,
    bounded: BoundedModelProjection,
    passthrough_response: bool,
    preserve_non_text_content: bool,
    pub(super) candidate: Option<crate::tool_history::ToolHistoryCandidate>,
    pub(super) projected_tokens: u64,
    pub(super) canonical_bytes: u64,
    pub(super) canonical_sha256: String,
    pub(super) canonical_tokens: u64,
    pub(super) model_bytes: u64,
    pub(super) artifact_created: bool,
    pub(super) artifact_reused: bool,
    pub(super) projection_truncated: bool,
    pub(super) omitted_sections: u64,
    pub(super) deterministic_continuation_receipt: Option<TurnTimingDeterministicContinuationReceipt>,
    pub(super) deterministic_continuation_content: Vec<Value>,
    pub(super) source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    applied_token_limit: usize,
}

#[derive(Clone, Debug)]
enum BoundedModelProjection {
    Envelope {
        envelope: ToolProjectionV1,
        rendered: String,
    },
    Fallback {
        rendered: String,
    },
}

impl BoundedModelProjection {
    fn envelope(&self) -> Option<&ToolProjectionV1> {
        match self {
            Self::Envelope { envelope, .. } => Some(envelope),
            Self::Fallback { .. } => None,
        }
    }


    pub(super) fn rendered(&self) -> &str {
        match self {
            Self::Envelope { rendered, .. } | Self::Fallback { rendered, .. } => rendered,
        }
    }

    fn into_rendered(self) -> String {
        match self {
            Self::Envelope { rendered, .. } | Self::Fallback { rendered } => rendered,
        }
    }
}

impl ModelToolProjection {
    pub(super) fn response(&self) -> ResponseInputItem {
        if self.passthrough_response {
            return self.original_response.clone();
        }
        projected_response_item(
            self.original_response.clone(),
            self.bounded.rendered().to_string(),
            self.preserve_non_text_content,
        )
    }

    pub(super) fn into_response(self) -> ResponseInputItem {
        if self.passthrough_response {
            return self.original_response;
        }
        projected_response_item(
            self.original_response,
            self.bounded.into_rendered(),
            self.preserve_non_text_content,
        )
    }

    pub(super) fn merge_owner_drained_continuations(
        &mut self,
        continuations: Vec<PendingOwnerDrainedContinuation>,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        if continuations.is_empty() {
            return Vec::new();
        }
        let mut identity_counts = std::collections::HashMap::new();
        for continuation in &continuations {
            if valid_owner_drained_receipt(&continuation.receipt)
                && let Some(identity) = continuation.receipt.runtime_identity()
            {
                *identity_counts.entry(identity).or_insert(0_usize) += 1;
            }
        }
        let mut identities = std::collections::HashSet::with_capacity(continuations.len());
        let Some(mut envelope) = self.bounded.envelope().cloned() else {
            return Vec::new();
        };
        let projected_text = envelope.result["selected_text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut bounded = self.bounded.clone();
        let mut accepted = Vec::new();
        for continuation in continuations {
            if !valid_owner_drained_receipt(&continuation.receipt)
                || !continuation
                    .receipt
                    .runtime_identity()
                    .is_some_and(|identity| {
                        identity_counts.get(&identity) == Some(&1) && identities.insert(identity)
                    })
            {
                continue;
            }
            let mut candidate = envelope.clone();
            let mut preserved = candidate.result["preserved_content"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let preserved_start = preserved.len();
            preserved.extend(continuation.preserved_content.iter().cloned());
            candidate.result["preserved_content"] = Value::Array(preserved);
            let selected_text = without_preserved_recovery_text(
                &projected_text,
                candidate.result["preserved_content"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            );
            let accepted_candidate =
                [selected_text.as_str(), ""]
                    .into_iter()
                    .find_map(|selected_text| {
                        let candidate_bounded = serialize_projection_with_limit(
                            candidate.clone(),
                            selected_text,
                            self.applied_token_limit,
                        )?;
                        let accepted_envelope = candidate_bounded.envelope().cloned()?;
                        drained_content_survived(
                            &accepted_envelope,
                            preserved_start,
                            &continuation.preserved_content,
                        )
                        .then_some((candidate_bounded, accepted_envelope))
                    });
            if let Some((candidate_bounded, accepted_envelope)) = accepted_candidate {
                envelope = accepted_envelope;
                bounded = candidate_bounded;
                accepted.push(continuation.receipt);
            }
        }
        if accepted.is_empty() {
            return accepted;
        }
        self.bounded = bounded;
        let rendered = self.bounded.rendered().to_string();
        self.projected_tokens = approx_token_count(&rendered) as u64;
        self.model_bytes = rendered.len() as u64;
        if let Some(candidate) = &mut self.candidate {
            candidate.bounded_model_output = rendered;
        }
        accepted
    }
}

// A code-mode cell often prints the recovered selection itself. The owner
// envelope also preserves that selection, so remove only exact copies from
// the presentation string before adding the authoritative structured value.
fn without_preserved_recovery_text(selected: &str, preserved: &[Value]) -> String {
    let mut selected = selected.to_string();
    for value in preserved {
        if value.get("artifact_id").is_none() || value.get("results").is_none() {
            continue;
        }
        for serialized in [
            serde_json::to_string(value),
            serde_json::to_string_pretty(value),
        ]
        .into_iter()
        .flatten()
        {
            selected = selected.replace(&serialized, "");
        }
        let Some(results) = value["results"].as_array() else {
            continue;
        };
        for result in results {
            let texts = result
                .get("text")
                .and_then(Value::as_str)
                .into_iter()
                .chain(
                    result["value"]["hydrated_ranges"]
                        .as_array()
                        .filter(|_| {
                            result["selector"]["kind"] == "search" && result["status"] == "ok"
                        })
                        .into_iter()
                        .flatten()
                        .filter_map(|range| range["text"].as_str()),
                );
            for text in texts.filter(|text| !text.is_empty()).flat_map(|text| {
                std::iter::once(text.to_string()).chain(serde_json::to_string(text).ok())
            }) {
                // Only remove a complete printed selection, never a substring
                // of a cell's independent commentary or another result.
                let mut ranges = selected
                    .match_indices(text.as_str())
                    .filter_map(|(start, value)| {
                        let end = start + value.len();
                        ((start == 0
                            || text.starts_with('\n')
                            || selected[..start].ends_with('\n'))
                            && (end == selected.len()
                                || text.ends_with('\n')
                                || selected[end..].starts_with('\n')))
                        .then_some(start..end)
                    })
                    .collect::<Vec<_>>();
                for range in ranges.drain(..).rev() {
                    selected.replace_range(range, "");
                }
            }
        }
    }
    selected
}

pub(super) fn valid_owner_drained_receipt(receipt: &TurnTimingDeterministicContinuationReceipt) -> bool {
    !receipt.resource_identity_hash.is_empty()
        && !receipt.state_revision.is_empty()
        && receipt.runtime_identity().is_some()
        && receipt.suppressed_continuation_count > 0
}

pub(super) struct ModelProjectionInput {
    fragments: Vec<ToolOutputProjectionFragment>,
    outcome: ToolOutputOutcome,
    essential_inline: Value,
    origin_call_id: String,
    selection_facts: ProjectionSelectionFacts,
    applied_token_limit: usize,
    projected_text: String,
    preserved_content: Vec<Value>,
    non_text_tokens: usize,
    non_text_bytes: usize,
    codex_home: std::path::PathBuf,
    thread_id: String,
    tool_name: String,
    canonical: CanonicalToolResult,
    original_output_sha256: String,
    original_output_tokens: u64,
    original_output_text: String,
    invocation_sha256: Option<String>,
    semantic_class: String,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    projection_truncated: bool,
    /// The producer or truncation needs a durable canonical artifact.
    canonical_artifact_required: bool,
    predetermined_ranges: Vec<ToolOutputProjectionRange>,
    predetermined_json_pointers: Vec<ToolOutputProjectionJsonPointer>,
    original_response: ResponseInputItem,
    materialization: ProjectionMaterialization,
}

/// Admission-only outputs below this size stay inline. A completed-tool
/// history receipt is not materially smaller than the output it would replace,
/// so a durable artifact there only adds I/O and misleading artifact counts.
const LAZY_CANONICAL_ARTIFACT_MIN_BYTES: u64 = 4 * 1024;

fn admission_only_artifact_is_profitable(canonical: &CanonicalToolResult) -> bool {
    !canonical.complete || canonical.exact_bytes >= LAZY_CANONICAL_ARTIFACT_MIN_BYTES
}

async fn resolve_projection_source_dependencies<F>(
    turn_timing_state: &Arc<TurnTimingState>,
    authoritative_override: Option<
        std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    >,
    precomputed: Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>>,
    fallback: F,
) -> std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>
where
    F: FnOnce() -> std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
{
    if let Some(source_dependencies) = authoritative_override {
        // A PreToolUse rewrite invalidates analysis of the original payload.
        // Count the final-payload analysis as fallback work even though it was
        // performed immediately after rebuilding the invocation.
        turn_timing_state
            .record_projection_source_dependencies_fallback_async()
            .await;
        return source_dependencies;
    }
    match precomputed {
        Some(source_dependencies) => {
            turn_timing_state
                .record_projection_source_dependencies_reuse_async()
                .await;
            source_dependencies
        }
        None => {
            turn_timing_state
                .record_projection_source_dependencies_fallback_async()
                .await;
            fallback()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectionMaterialization {
    AdmissionOnly,
    CanonicalArtifact,
    InlineCarrier,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectionSelectionFacts {
    mode: &'static str,
    available_fragments: usize,
    selected_fragments: usize,
    exact_duplicates_removed: usize,
    selected_ids: Vec<String>,
    omitted_inline_ids: Vec<String>,
    partial_ids: Vec<String>,
}

pub(super) async fn prepare_model_projection(
    invocation: &ToolInvocation,
    result: &mut AnyToolResult,
    parsed_function_arguments: Option<&ParsedFunctionArguments>,
    source_dependencies_override: Option<
        &std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    >,
    force_inline_carrier: bool,
    track_for_admission: bool,
) -> Option<ModelProjectionInput> {
    // JS consumes the native nested result, not this hidden projection. Its
    // outer cell and nested-evidence owner retain the actually visible packet.
    if !projection_is_provider_visible(&invocation.source)
        && !force_inline_carrier
        && !result.result.requires_canonical_artifact()
    {
        return None;
    }
    // Exact artifact reads are already bounded and must never recursively spill.
    // Code mode also performs its own coherent outer projection after merging
    // native nested results. Keep the first `exec`/`wait` result byte-for-byte
    // provider-visible, but admit it to completed-tool history so later
    // generations can replace consumed output with an exact-artifact receipt.
    let admit_code_mode_output =
        crate::tools::code_mode::is_orchestration_tool_name(&invocation.tool_name)
            && !force_inline_carrier
            && track_for_admission;
    if generic_projection_is_exempt(&invocation.tool_name, force_inline_carrier)
        && !admit_code_mode_output
    {
        return None;
    }

    let (mut original_response, producer_metadata) = result
        .result
        .to_response_item_with_projection_metadata(&result.call_id, &result.payload);
    let preserved_content = preserved_non_text_content(&original_response);
    let using_admission_fallback = producer_metadata.is_none();
    let metadata = producer_metadata.or_else(|| {
        track_for_admission.then(|| {
            admission_fallback_metadata(&original_response, result.result.outcome_for_logging())
        })?
    })?;
    // A yielded cell still owns a live handle that the model may need on the
    // next generation. Only terminal code-mode output is safe to retire.
    let has_running_nested_command = metadata
        .essential_inline
        .get("nested_commands")
        .and_then(Value::as_array)
        .is_some_and(|states| {
            states
                .iter()
                .any(crate::tools::code_mode::command_owns_work)
        });
    if admit_code_mode_output
        && (metadata.outcome == ToolOutputOutcome::Yielded || has_running_nested_command)
    {
        return None;
    }
    let spillable_text = metadata.spillable_text.join("\n");
    if spillable_text.is_empty() && preserved_content.is_empty() && metadata.fragments.is_empty() {
        return None;
    }
    let outcome = match metadata.outcome {
        ToolOutputOutcome::Success => OutputOutcome::Success,
        ToolOutputOutcome::Failure => OutputOutcome::Failure,
        ToolOutputOutcome::TimedOut => OutputOutcome::TimedOut,
        ToolOutputOutcome::Yielded => OutputOutcome::Success,
        ToolOutputOutcome::Skipped => OutputOutcome::Skipped,
    };
    let diagnostic_class = match metadata.diagnostic_class {
        ToolOutputDiagnosticClass::Normal => OutputDiagnosticClass::Normal,
        ToolOutputDiagnosticClass::HighSignal => OutputDiagnosticClass::HighSignal,
    };
    let requested_limit = metadata.requested_limit;
    let hard_limit = invocation
        .step_context
        .model_output_token_limit(
            &invocation.session,
            if matches!(
                invocation.tool_name.name.as_str(),
                "exec_command" | "write_stdin" | "read_file"
            ) {
                10_000
            } else {
                8_000
            },
        )
        .await;
    let limits =
        resolve_projected_output_limits(requested_limit, outcome, diagnostic_class, hard_limit);
    // This pass only decides whether canonical recovery is needed. The actual
    // projection is rendered below; constructing and discarding a truncated
    // copy here repeats the most expensive work on oversized results.
    let exceeds_limit =
        codex_utils_string::approx_token_count_exceeds(&spillable_text, limits.applied_limit);
    let non_text_tokens = non_text_projection_token_cost(&preserved_content);
    // Truncation already proves the canonical text exceeds the applied budget.
    // Avoid a second full-buffer token scan on the large-output path; the
    // canonical producer remains the accounting source for the artifact.
    let model_output_tokens = if exceeds_limit {
        limits.applied_limit.saturating_add(1)
    } else {
        approx_token_count(&spillable_text)
    }
    .saturating_add(non_text_tokens);
    let projection_truncated = exceeds_limit || model_output_tokens > limits.applied_limit;
    if !result.result.allows_generic_artifact_retention() {
        if projection_truncated {
            let model_visible = FunctionToolOutput::from_text(
                "Private tool execution completed, but result delivery is unavailable at this size. No generic recovery artifact was created. Use a narrower read, line/character range, or search; do not repeat a mutation merely to recover its response.".to_string(),
                Some(result.success_for_logging()),
            );
            let original = std::mem::replace(
                &mut result.result,
                Box::new(FunctionToolOutput::from_text(String::new(), None)),
            );
            result.result = Box::new(UnavailableModelProjectionOutput {
                original,
                model_visible,
            });
        }
        // Do not admit even fitting private attachments into completed-tool
        // recovery storage. Their producer-owned canonical packet is not a
        // grant of access to code mode or generic read_tool_output.
        return None;
    }
    // Preset selectors describe how to recover omitted evidence; they do not
    // establish that anything was omitted. Projecting a fitting result solely
    // for these hints needlessly reads its artifact and, in code mode, merges
    // recovered excerpts alongside the complete native result printed by JS.
    let needs_canonical_artifact = result.result.requires_canonical_artifact()
        || projection_truncated
        || metadata
            .essential_inline
            .get(crate::tools::code_mode::VISIBLE_OUTPUT_TRUNCATED_KEY)
            == Some(&Value::Bool(true));
    let retain_for_history = track_for_admission
        && invocation
            .step_context
            .turn
            .config
            .completed_tool_history_projection;
    if !needs_canonical_artifact && !force_inline_carrier && !retain_for_history {
        return None;
    }
    let materialization = if force_inline_carrier {
        ProjectionMaterialization::InlineCarrier
    } else if admit_code_mode_output {
        ProjectionMaterialization::AdmissionOnly
    } else if needs_canonical_artifact {
        ProjectionMaterialization::CanonicalArtifact
    } else {
        ProjectionMaterialization::AdmissionOnly
    };
    // An owner result is the last host boundary for deterministic nested
    // continuations. Give that coherent packet the full model-safe budget so
    // retained continuation evidence is merged before the model is sampled
    // again. Ordinary per-result projection limits apply only after this
    // generation-level packet has been formed.
    let applied_token_limit = projection_packet_token_limit(
        force_inline_carrier,
        limits.applied_limit,
        limits.hard_limit,
    );
    let (typed_projected_text, selection_facts) =
        if materialization == ProjectionMaterialization::InlineCarrier {
            (
                None,
                ProjectionSelectionFacts {
                    mode: "inline_continuation_carrier",
                    available_fragments: metadata.fragments.len(),
                    selected_fragments: metadata.fragments.len(),
                    exact_duplicates_removed: 0,
                    selected_ids: metadata
                        .fragments
                        .iter()
                        .filter_map(|fragment| fragment.id.clone())
                        .collect(),
                    omitted_inline_ids: Vec::new(),
                    partial_ids: Vec::new(),
                },
            )
        } else if metadata.fragments.is_empty() {
            (
                None,
                ProjectionSelectionFacts {
                    mode: "generic_fallback",
                    available_fragments: 0,
                    selected_fragments: 0,
                    exact_duplicates_removed: 0,
                    selected_ids: Vec::new(),
                    omitted_inline_ids: Vec::new(),
                    partial_ids: Vec::new(),
                },
            )
        } else {
            // Select from original fragments only after the rendered header is sized.
            let (text, facts) = select_typed_projection_fragments(&metadata.fragments, usize::MAX);
            (Some(text), facts)
        };

    let mut canonical = result
        .result
        .canonical_result(&result.payload)
        .or_else(|| {
            using_admission_fallback.then(|| {
                admission_fallback_canonical_result(
                    &spillable_text,
                    &preserved_content,
                    result.result.code_mode_result(&result.payload),
                )
            })
        })?;
    let original_output_text = if materialization == ProjectionMaterialization::AdmissionOnly {
        let (normalized_response, text) = normalize_admission_response(original_response)?;
        original_response = normalized_response;
        text
    } else {
        history_output_text(&original_response)
            .unwrap_or_else(|| consolidated_history_output_text(&original_response))
    };
    canonical.sections = if materialization == ProjectionMaterialization::InlineCarrier {
        Vec::new()
    } else {
        canonical_projection_sections(
            &canonical,
            &metadata.fragments,
            &selection_facts,
            &metadata.predetermined_ranges,
            &metadata.predetermined_json_pointers,
        )
    };
    let validation_material = metadata.fragments.iter().any(|fragment| {
        fragment.kind == ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary
    });
    let semantic_class = if validation_material {
        "validation"
    } else {
        match metadata.outcome {
            ToolOutputOutcome::Failure => "tool_failure",
            ToolOutputOutcome::TimedOut => "tool_timeout",
            ToolOutputOutcome::Success
            | ToolOutputOutcome::Yielded
            | ToolOutputOutcome::Skipped => "tool_output",
        }
    };
    let native_cwd = crate::tools::parallel::native_workspace_analysis_cwd(
        &invocation.step_context, &invocation.payload,
    );
    let source_dependencies = resolve_projection_source_dependencies(
        &invocation.step_context.turn.turn_timing_state,
        source_dependencies_override.cloned(),
        precomputed_projection_source_dependencies(),
        || {
            crate::tool_history::source_dependencies_for_tool_call_with_parsed_arguments(
                flat_tool_name(&invocation.tool_name).as_ref(),
                &invocation.payload,
                parsed_function_arguments.and_then(|parsed| parsed.value().ok()),
                native_cwd.as_ref()
                    .unwrap_or(&invocation.step_context.turn.config.cwd).as_path(),
                native_cwd.is_some(),
            )
        },
    )
    .await;
    let original_output_sha256 = crate::tool_history::sha256(original_output_text.as_bytes());
    let original_output_tokens = if exceeds_limit {
        canonical
            .approximate_tokens
            .saturating_add(non_text_tokens as u64)
    } else {
        model_output_tokens as u64
    };
    let invocation_sha256 = authorized_tool_invocation_sha256(
        &invocation.step_context.turn,
        &invocation.payload,
        parsed_function_arguments,
    );
    Some(ModelProjectionInput {
        fragments: metadata.fragments,
        outcome: metadata.outcome,
        essential_inline: metadata.essential_inline,
        origin_call_id: result.call_id.clone(),
        selection_facts,
        applied_token_limit,
        projected_text: typed_projected_text.unwrap_or(spillable_text),
        non_text_tokens,
        non_text_bytes: non_text_projection_byte_cost(&preserved_content),
        preserved_content,
        codex_home: invocation.step_context.turn.config.codex_home.to_path_buf(),
        thread_id: invocation.session.thread_id.to_string(),
        tool_name: flat_tool_name(&invocation.tool_name).into_owned(),
        canonical,
        original_output_sha256,
        original_output_tokens,
        original_output_text,
        invocation_sha256,
        semantic_class: semantic_class.to_string(),
        source_dependencies,
        projection_truncated,
        canonical_artifact_required: needs_canonical_artifact,
        predetermined_ranges: metadata.predetermined_ranges,
        predetermined_json_pointers: metadata.predetermined_json_pointers,
        original_response,
        materialization,
    })
}

fn admission_fallback_metadata(
    response: &ResponseInputItem,
    outcome: ToolOutputOutcome,
) -> Option<ToolOutputProjectionMetadata> {
    match response {
        ResponseInputItem::FunctionCallOutput { .. }
        | ResponseInputItem::CustomToolCallOutput { .. }
        | ResponseInputItem::McpToolCallOutput { .. } => {}
        ResponseInputItem::Message { .. } | ResponseInputItem::ToolSearchOutput { .. } => {
            return None;
        }
    }
    let spillable_text = consolidated_history_output_text(response);
    if spillable_text.is_empty() && preserved_non_text_content(response).is_empty() {
        return None;
    }
    Some(ToolOutputProjectionMetadata {
        outcome,
        diagnostic_class: ToolOutputDiagnosticClass::Normal,
        fragments: Vec::new(),
        spillable_text: vec![spillable_text],
        essential_inline: serde_json::json!({}),
        requested_limit: None,
        predetermined_ranges: Vec::new(),
        predetermined_json_pointers: Vec::new(),
    })
}

fn admission_fallback_canonical_result(
    spillable_text: &str,
    preserved_content: &[Value],
    code_mode_result: Value,
) -> CanonicalToolResult {
    if preserved_content.is_empty() {
        CanonicalToolResult::text(spillable_text)
    } else {
        CanonicalToolResult::json(code_mode_result)
    }
}

fn single_function_text(items: &[FunctionCallOutputContentItem]) -> Option<&str> {
    let mut texts = items.iter().filter_map(|item| match item {
        FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
        FunctionCallOutputContentItem::InputImage { .. }
        | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
    });
    let text = texts.next()?;
    texts.next().is_none().then_some(text)
}

fn normalize_admission_response(
    original: ResponseInputItem,
) -> Option<(ResponseInputItem, String)> {
    if let Some(text) = history_output_text(&original) {
        return Some((original, text));
    }
    let text = consolidated_history_output_text(&original);
    // Media-bearing packets must retain text/image positions at admission.
    // Their later retirement already checks for non-text siblings.
    if !preserved_non_text_content(&original).is_empty() {
        return Some((original, text));
    }
    let normalized =
        projected_response_item(original, text, /*preserve_non_text_content*/ true);
    history_output_text(&normalized).map(|normalized_text| (normalized, normalized_text))
}

pub(super) fn history_output_text(response: &ResponseInputItem) -> Option<String> {
    match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => {
            history_output_body_text(&output.body)
        }
        ResponseInputItem::McpToolCallOutput { output, .. } => {
            history_output_body_text(&output.as_function_call_output_payload().body)
        }
        _ => None,
    }
}

fn history_output_body_text(body: &FunctionCallOutputBody) -> Option<String> {
    match body {
        FunctionCallOutputBody::Text(text) => Some(text.clone()),
        FunctionCallOutputBody::ContentItems(items) => {
            single_function_text(items).map(str::to_string)
        }
    }
}

pub(super) fn consolidated_history_output_text(response: &ResponseInputItem) -> String {
    let body = match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output.body.clone(),
        ResponseInputItem::McpToolCallOutput { output, .. } => {
            output.as_function_call_output_payload().body
        }
        _ => return String::new(),
    };
    match body {
        FunctionCallOutputBody::Text(text) => text,
        FunctionCallOutputBody::ContentItems(items) => items
            .into_iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text),
                FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

pub(super) fn generic_projection_is_exempt(tool_name: &ToolName, force_inline_carrier: bool) -> bool {
    (tool_name.namespace.is_none() && tool_name.name == "read_tool_output")
        // MCP handlers already own provider-safe truncation and typed content
        // shaping. Re-projecting their results can replace image content with
        // a text artifact and can supersede distinct parallel call/output
        // pairs when their arguments and results happen to be identical.
        || tool_name
            .namespace
            .as_deref()
            .is_some_and(|namespace| namespace.starts_with("mcp__"))
        || (crate::tools::code_mode::is_orchestration_tool_name(tool_name) && !force_inline_carrier)
}

fn projection_packet_token_limit(
    has_owner_drained_continuations: bool,
    ordinary_limit: usize,
    hard_limit: usize,
) -> usize {
    if has_owner_drained_continuations {
        hard_limit
    } else {
        ordinary_limit
    }
}

fn canonical_projection_sections(
    canonical: &CanonicalToolResult,
    fragments: &[ToolOutputProjectionFragment],
    selection: &ProjectionSelectionFacts,
    predetermined_ranges: &[ToolOutputProjectionRange],
    predetermined_json_pointers: &[ToolOutputProjectionJsonPointer],
) -> Vec<ToolProjectionSection> {
    let selected = selection.selected_ids.iter().collect::<HashSet<_>>();
    let partial = selection.partial_ids.iter().collect::<HashSet<_>>();
    let declared_ranges = predetermined_ranges
        .iter()
        .filter_map(|range| {
            canonical_line_range(canonical, range.start_line, range.end_line)
                .map(|canonical_range| (range.id.as_str(), canonical_range))
        })
        .chain(predetermined_json_pointers.iter().filter_map(|selector| {
            canonical
                .json_pointers
                .get(&selector.pointer)
                .map(|pointer| (selector.id.as_str(), pointer.range))
        }))
        .collect::<HashMap<_, _>>();
    let mut cursor = 0_usize;
    let mut sections = fragments
        .iter()
        .enumerate()
        .filter_map(|(index, fragment)| {
            let id = fragment
                .id
                .clone()
                .unwrap_or_else(|| format!("fragment:{index}"));
            let range = declared_ranges.get(id.as_str()).copied().or_else(|| {
                if fragment.text.is_empty() {
                    return None;
                }
                canonical.bytes[cursor..]
                    .windows(fragment.text.len())
                    .position(|window| window == fragment.text.as_bytes())
                    .map(|offset| {
                        let start = cursor + offset;
                        let end = start + fragment.text.len();
                        cursor = end;
                        CanonicalByteRange::new(start as u64, end as u64)
                    })
            })?;
            Some(ToolProjectionSection {
                exact_bytes: range.len(),
                inclusion: if selected.contains(&id) && !partial.contains(&id) {
                    ToolProjectionInclusion::Included
                } else {
                    ToolProjectionInclusion::Omitted
                },
                // `selected_text` is the sole inline copy. Section metadata is
                // an address directory, never a second payload copy.
                value: None,
                canonical_range: Some(range),
                children: Vec::new(),
                recovery_chunk_bytes: None,
                id,
            })
        })
        .collect::<Vec<_>>();

    let mut section_ids = sections
        .iter()
        .map(|section| section.id.clone())
        .collect::<HashSet<_>>();
    for range in predetermined_ranges {
        if section_ids.insert(range.id.clone())
            && let Some(canonical_range) =
                canonical_line_range(canonical, range.start_line, range.end_line)
        {
            sections.push(omitted_projection_section(
                range.id.clone(),
                canonical_range,
            ));
        }
    }
    for selector in predetermined_json_pointers {
        if section_ids.insert(selector.id.clone())
            && let Some(pointer) = canonical.json_pointers.get(&selector.pointer)
        {
            sections.push(omitted_projection_section(
                selector.id.clone(),
                pointer.range,
            ));
        }
    }

    if canonical.kind == CanonicalToolResultKind::Json {
        const MAX_TOP_LEVEL_JSON_SECTIONS: usize = 64;
        if let Some(root) = canonical.json_pointers.get("") {
            for pointer in root
                .direct_children
                .iter()
                .take(MAX_TOP_LEVEL_JSON_SECTIONS)
            {
                let id = format!("json:{pointer}");
                if section_ids.insert(id.clone())
                    && let Some(entry) = canonical.json_pointers.get(pointer)
                {
                    sections.push(omitted_projection_section(id, entry.range));
                }
            }
        }
    }

    if sections.is_empty() {
        sections.push(omitted_projection_section(
            "result".to_string(),
            CanonicalByteRange::new(0, canonical.exact_bytes),
        ));
    }
    sections
}

fn canonical_line_range(
    canonical: &CanonicalToolResult,
    start_line: usize,
    end_line: usize,
) -> Option<CanonicalByteRange> {
    if start_line == 0 || end_line < start_line {
        return None;
    }
    let mut line_starts = vec![0_u64];
    line_starts.extend(
        canonical
            .bytes
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index as u64 + 1))
            .filter(|offset| *offset < canonical.exact_bytes),
    );
    let start = *line_starts.get(start_line - 1)?;
    let end = line_starts
        .get(end_line)
        .copied()
        .unwrap_or(canonical.exact_bytes);
    Some(CanonicalByteRange::new(start, end))
}

fn omitted_projection_section(
    id: String,
    canonical_range: CanonicalByteRange,
) -> ToolProjectionSection {
    ToolProjectionSection {
        id,
        value: None,
        exact_bytes: canonical_range.len(),
        inclusion: ToolProjectionInclusion::Omitted,
        canonical_range: Some(canonical_range),
        children: Vec::new(),
        recovery_chunk_bytes: None,
    }
}

const PROJECTION_FRAGMENT_KIND_ORDER: [ToolOutputProjectionFragmentKind; 4] = [
    ToolOutputProjectionFragmentKind::ErrorOrDiagnostic,
    ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary,
    ToolOutputProjectionFragmentKind::ProcessFinalStatus,
    ToolOutputProjectionFragmentKind::ContextualSpillableText,
];

fn select_typed_projection_fragments(
    fragments: &[ToolOutputProjectionFragment],
    token_limit: usize,
) -> (String, ProjectionSelectionFacts) {
    let mut seen = HashSet::new();
    let unique = fragments
        .iter()
        .filter(|fragment| seen.insert(*fragment))
        .collect::<Vec<_>>();
    let exact_duplicates_removed = fragments.len().saturating_sub(unique.len());
    let active_kinds = PROJECTION_FRAGMENT_KIND_ORDER
        .iter()
        .filter(|kind| unique.iter().any(|fragment| fragment.kind == **kind))
        .count();
    let active_headings = PROJECTION_FRAGMENT_KIND_ORDER
        .iter()
        .filter(|kind| unique.iter().any(|fragment| fragment.kind == **kind))
        .map(|kind| fragment_section_heading(*kind))
        .collect::<Vec<_>>();
    // Approximate-token counting carries per-call framing overhead. Count the
    // headings as the single string they will become so eleven small typed
    // sections do not incorrectly consume the entire budget before payloads.
    let headings_tokens = approx_token_count(&active_headings.join("\n\n"));
    let section_budget = token_limit
        .saturating_sub(headings_tokens)
        .checked_div(active_kinds.max(1))
        .unwrap_or(0);
    let demands = PROJECTION_FRAGMENT_KIND_ORDER.map(|kind| {
        unique
            .iter()
            .filter(|fragment| fragment.kind == kind)
            .map(|fragment| approx_token_count(&fragment.text).saturating_add(1))
            .sum::<usize>()
    });
    let mut allocations = demands.map(|demand| demand.min(section_budget));
    let mut spare = token_limit
        .saturating_sub(headings_tokens)
        .saturating_sub(allocations.iter().sum::<usize>());
    for (allocation, demand) in allocations.iter_mut().zip(demands) {
        let extra = spare.min(demand.saturating_sub(*allocation));
        *allocation += extra;
        spare -= extra;
    }
    let mut sections = Vec::new();
    let mut selected_fragments = 0;
    let mut selected_ids = Vec::new();
    let mut partial_ids = Vec::new();

    for (kind, allocation) in PROJECTION_FRAGMENT_KIND_ORDER.into_iter().zip(allocations) {
        let section_fragments = unique
            .iter()
            .filter(|fragment| fragment.kind == kind)
            .collect::<Vec<_>>();
        if section_fragments.is_empty() {
            continue;
        }
        let mut remaining_budget = allocation;
        let mut bounded_fragments = Vec::new();
        for fragment in section_fragments {
            if remaining_budget == 0 {
                break;
            }
            let separator_tokens = usize::from(!bounded_fragments.is_empty());
            if remaining_budget <= separator_tokens {
                break;
            }
            remaining_budget = remaining_budget.saturating_sub(separator_tokens);
            let bounded = if token_limit == usize::MAX {
                fragment.text.clone()
            } else if let Some(source) = &fragment.shell_output {
                crate::tools::shell_output_summary::summarize_shell_output_for_model_with_streams(
                    &fragment.text,
                    source.exit_code,
                    false,
                    crate::tools::shell_output_summary::ShellOutputSummaryOptions {
                        enabled: true,
                        applied_token_limit: Some(remaining_budget),
                        command_text: source.command_text.as_deref(),
                    },
                    source
                        .streams
                        .as_ref()
                        .map(|(stdout, stderr)| (stdout.as_str(), stderr.as_str())),
                )
                .unwrap_or_else(|| {
                    codex_utils_output_truncation::truncate_text_with_line_markers(
                        &fragment.text,
                        remaining_budget,
                    )
                })
            } else if fragment.id.as_deref() == Some("output")
                && fragment.kind == ToolOutputProjectionFragmentKind::ContextualSpillableText
            {
                codex_utils_output_truncation::truncate_text_with_line_markers(
                    &fragment.text,
                    remaining_budget,
                )
            } else {
                bounded_fragment_text(&fragment.text, remaining_budget)
            };
            if bounded.is_empty() {
                continue;
            }
            remaining_budget = remaining_budget.saturating_sub(approx_token_count(&bounded));
            if let Some(id) = &fragment.id {
                selected_ids.push(id.clone());
                if bounded != fragment.text {
                    partial_ids.push(id.clone());
                }
            }
            bounded_fragments.push(bounded);
        }
        if bounded_fragments.is_empty() {
            continue;
        }
        selected_fragments += bounded_fragments.len();
        sections.push(format!(
            "{}\n{}",
            fragment_section_heading(kind),
            bounded_fragments.join("\n")
        ));
    }

    let selected_text = sections.join("\n\n");
    let selected_tokens = approx_token_count(&selected_text);
    if selected_tokens > token_limit && token_limit > 0 {
        return select_typed_projection_fragments(
            fragments,
            token_limit.saturating_sub(selected_tokens - token_limit),
        );
    }
    let selected_id_set = selected_ids.iter().collect::<HashSet<_>>();
    let omitted_inline_ids = unique
        .iter()
        .filter_map(|fragment| fragment.id.as_ref())
        .filter(|id| !selected_id_set.contains(id))
        .cloned()
        .collect();
    (
        selected_text,
        ProjectionSelectionFacts {
            mode: "typed_fragments",
            available_fragments: fragments.len(),
            selected_fragments,
            exact_duplicates_removed,
            selected_ids,
            omitted_inline_ids,
            partial_ids,
        },
    )
}

fn bounded_fragment_text(text: &str, token_budget: usize) -> String {
    let bounded = truncate_text_to_token_ceiling(text, token_budget);
    if !bounded.is_empty() || token_budget == 0 || text.is_empty() {
        return bounded;
    }
    // The generic middle-truncation marker can consume an extremely small
    // fair-share budget. Typed projection still needs one whole, attributable
    // fragment per active section, so fall back to the longest exact prefix
    // that fits without a marker.
    let mut prefix = String::new();
    for ch in text.chars() {
        prefix.push(ch);
        if approx_token_count(&prefix) > token_budget {
            prefix.pop();
            break;
        }
    }
    prefix
}

fn fragment_section_heading(kind: ToolOutputProjectionFragmentKind) -> &'static str {
    match kind {
        ToolOutputProjectionFragmentKind::ErrorOrDiagnostic => "[errors and diagnostics]",
        ToolOutputProjectionFragmentKind::ValidationFailureOrFinalSummary => "[validation]",
        ToolOutputProjectionFragmentKind::ProcessFinalStatus => "[process final status]",
        ToolOutputProjectionFragmentKind::ContextualSpillableText => "[context]",
    }
}

/// Room a truncated-cell recovery page leaves in a default cell for the read
/// result's envelope and the script's own status lines.
const CELL_RECOVERY_PAGE_RESERVE_TOKENS: usize = 2_000;
/// Pages one `cell_recovery` read delivers together: a nested result keeps 4/5
/// of the exec ceiling, less the reserve for its envelope. A notice names at
/// most this many and leaves the rest to `continuation`.
const CELL_RECOVERY_MAX_PAGES: usize = {
    let pages = (codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL * 4 / 5
        - CELL_RECOVERY_PAGE_RESERVE_TOKENS)
        / (codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL
            - CELL_RECOVERY_PAGE_RESERVE_TOKENS);
    if pages == 0 { 1 } else { pages }
};

struct CellRecoveryPages {
    selectors: Vec<Value>,
    /// Total bytes of `selectors`, so one read delivers every page.
    max_bytes: usize,
    continuation: Option<Value>,
}

/// Selectors one notice may name when several truncated results each leave a
/// small gap; their total size stays within `CELL_RECOVERY_MAX_PAGES` pages.
const CELL_RECOVERY_MAX_SELECTORS: usize = 16;

/// Recorded recoveries of truncated cells read the omitted lines whole and were
/// cut by the cell budget again (audit 2026-10-22 #45). Split the omitted range
/// of the saved output into contiguous byte pages that each print within a
/// default cell. Models then read one page per generation and rarely raised
/// the cell budget (audit 2026-10-28 #3), so one `cell_recovery` read takes all.
/// When several results were cut, `ranges` names each gap and pages skip the
/// lines shown between them.
fn cell_recovery_pages(
    canonical: &CanonicalToolResult,
    selector: &Value,
) -> Option<CellRecoveryPages> {
    if selector.get("kind").and_then(Value::as_str) != Some("lines")
        || u64::try_from(canonical.bytes.len()).ok() != Some(canonical.exact_bytes)
    {
        return None;
    }
    let line = |value: Option<&Value>| usize::try_from(value?.as_u64()?).ok();
    let ranges = match selector.get("ranges").and_then(Value::as_array) {
        // `[first, last]` pairs from `omitted_line_selector`.
        Some(ranges) => ranges
            .iter()
            .map(|range| Some((line(range.get(0))?, line(range.get(1))?)))
            .collect::<Option<Vec<_>>>()?,
        None => vec![(line(selector.get("start"))?, line(selector.get("end"))?)],
    };
    let text = std::str::from_utf8(&canonical.bytes).ok()?;
    // Byte offset where the 1-based `line` begins.
    let line_start = |line: usize| {
        if line == 1 {
            Some(0)
        } else {
            text.match_indices('\n')
                .nth(line - 2)
                .map(|(index, _)| index + 1)
        }
    };
    let mut spans = Vec::with_capacity(ranges.len());
    for (first, last) in ranges {
        if first == 0 || last < first {
            return None;
        }
        let start = line_start(first)?;
        let end = line_start(last.saturating_add(1)).unwrap_or(text.len());
        if start < end {
            spans.push((start, end));
        }
    }
    let end = spans.last()?.1;
    let page_tokens = codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL
        .saturating_sub(CELL_RECOVERY_PAGE_RESERVE_TOKENS)
        .max(1);
    // One read carries what `CELL_RECOVERY_MAX_PAGES` full pages would; small
    // gaps share that room instead of each claiming a whole page.
    let budget_tokens = page_tokens.saturating_mul(CELL_RECOVERY_MAX_PAGES);
    let mut used_tokens = 0_usize;
    let mut selectors = Vec::new();
    let mut max_bytes = 0;
    let mut resume_at = None;
    'spans: for (span_start, span_end) in spans {
        let mut page_start = span_start;
        while page_start < span_end {
            let page_end = cell_recovery_page_end(text, page_start, span_end, page_tokens);
            let tokens = cell_recovery_page_tokens(&text[page_start..page_end]);
            if !selectors.is_empty()
                && (selectors.len() >= CELL_RECOVERY_MAX_SELECTORS
                    || used_tokens.saturating_add(tokens) > budget_tokens)
            {
                resume_at = Some(page_start);
                break 'spans;
            }
            used_tokens = used_tokens.saturating_add(tokens);
            max_bytes += page_end - page_start;
            selectors.push(serde_json::json!({"kind":"bytes", "start":page_start, "end":page_end}));
            page_start = page_end;
        }
    }
    // The rest is one contiguous selector; it may span shown lines between
    // later gaps, which only happens beyond a full recovery read.
    let continuation =
        resume_at.map(|start| serde_json::json!({"kind":"bytes", "start":start, "end":end}));
    Some(CellRecoveryPages {
        selectors,
        max_bytes,
        continuation,
    })
}

/// Model tokens of one recovery page as `read_tool_output` returns it: JSON-escaped text.
fn cell_recovery_page_tokens(page: &str) -> usize {
    serde_json::to_string(page).map_or(usize::MAX, |escaped| {
        codex_utils_output_truncation::model_token_count(&escaped)
    })
}

/// End of the page that starts at `start`: the last line boundary (or, inside
/// one long line, a character boundary) whose JSON-escaped text stays within
/// `page_tokens`. Prose averages about four UTF-8 bytes per token, so begin
/// there and shrink in proportion until the page fits.
fn cell_recovery_page_end(text: &str, start: usize, end: usize, page_tokens: usize) -> usize {
    let mut candidate = end.min(start.saturating_add(page_tokens.saturating_mul(4)));
    loop {
        let page_end = if candidate >= end {
            end
        } else {
            let mut boundary = candidate;
            while boundary > start && !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            match text[start..boundary].rfind('\n') {
                Some(newline) => start + newline + 1,
                None if boundary > start => boundary,
                // Always make progress: at least one whole character.
                None => text[start..end]
                    .chars()
                    .next()
                    .map_or(end, |ch| start + ch.len_utf8()),
            }
        };
        let page = &text[start..page_end];
        let tokens = cell_recovery_page_tokens(page);
        if tokens <= page_tokens || page.chars().nth(1).is_none() {
            return page_end;
        }
        let length = page_end - start;
        let scaled = (length as u128 * page_tokens as u128 * 9 / (tokens as u128 * 10)) as usize;
        candidate = start + scaled.clamp(1, length - 1);
    }
}

fn cell_recovery_note(pages: usize, has_continuation: bool) -> String {
    format!(
        "One call with these arguments reads all {pages} omitted pages, not lines already \
         shown; cell_recovery gives them room in that cell beyond its default output budget.{}",
        if has_continuation {
            " Then read `continuation` with the same max_bytes and cell_recovery."
        } else {
            ""
        }
    )
}

pub(super) async fn project_model_output(input: ModelProjectionInput) -> Option<ModelToolProjection> {
    let ModelProjectionInput {
        fragments,
        outcome,
        essential_inline,
        origin_call_id,
        selection_facts,
        mut applied_token_limit,
        projected_text,
        mut preserved_content,
        non_text_tokens,
        non_text_bytes,
        codex_home,
        thread_id,
        tool_name,
        canonical,
        original_output_sha256,
        original_output_tokens,
        original_output_text,
        invocation_sha256,
        semantic_class,
        source_dependencies,
        projection_truncated,
        canonical_artifact_required,
        predetermined_ranges,
        predetermined_json_pointers,
        original_response,
        materialization,
    } = input;
    let canonical_sha256 = canonical.sha256.clone();
    let successful = matches!(&outcome, ToolOutputOutcome::Success);
    let total_applied_token_limit = applied_token_limit;
    // Make the canonical artifact durable before spending time on the inline
    // projection. If projection later fails or is cancelled, recovery still
    // has the complete canonical output to work from.
    // Small, complete admission-only outputs stay inline: nothing needs to
    // recover them and a receipt would not be smaller than the output.
    let lazy_inline_admission = materialization == ProjectionMaterialization::AdmissionOnly
        && !canonical_artifact_required
        && !admission_only_artifact_is_profitable(&canonical);
    let persisted_artifact =
        if materialization == ProjectionMaterialization::InlineCarrier || lazy_inline_admission {
            None
        } else {
            let existing_artifact_id = essential_inline
                .get("raw_output_artifact_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut artifact_created = existing_artifact_id.is_none();
            let artifact = if let Some(artifact_id) = existing_artifact_id {
                let attached = attach_canonical_output_artifact(&codex_home, &thread_id, &artifact_id, &canonical)
                    .await;
            if attached.complete && attached.retained_bytes == canonical.exact_bytes {
                attached
            } else {
                // A cumulative stream and this poll's packet are different
                // coordinate spaces. Never weaken exact-byte attachment.
                artifact_created = true;
                crate::tools::command_output_artifact::create_tool_history_output_artifact(
                    &codex_home,
                    &thread_id,
                    &canonical,
                )
                .await
            }
        } else {
            crate::tools::command_output_artifact::create_tool_history_output_artifact(
                &codex_home,
                &thread_id,
                &canonical,
            )
            .await
        };
        let artifact_id = artifact.artifact_id();
        if let Some(artifact_id) = artifact_id
            && artifact.complete
            && artifact.retained_bytes == canonical.exact_bytes
            && artifact.unavailable_ranges.is_empty()
            && (artifact_created
                || crate::tools::command_output_artifact::protect_active_tool_history_artifact(
                    &codex_home,
                    &thread_id,
                    &artifact_id,
                    canonical.exact_bytes,
                    &canonical.sha256,
                )
                .await
                .is_ok())
        {
            Some((artifact, artifact_id, artifact_created))
        } else {
            None
        }
    };
    let preserve_non_text_content = !preserved_content.is_empty()
        && non_text_tokens <= applied_token_limit.saturating_sub(MIN_PROJECTION_ENVELOPE_TOKENS);
    let retained_non_text_tokens = if preserve_non_text_content {
        non_text_tokens
    } else {
        0
    };
    let retained_non_text_bytes = if preserve_non_text_content {
        non_text_bytes
    } else {
        0
    };
    if preserve_non_text_content {
        preserved_content.clear();
        applied_token_limit = applied_token_limit.saturating_sub(non_text_tokens).max(1);
    }
    let outcome = match outcome {
        ToolOutputOutcome::Success => "success",
        ToolOutputOutcome::Failure => "failure",
        ToolOutputOutcome::TimedOut => "timeout",
        ToolOutputOutcome::Yielded => "yielded",
        ToolOutputOutcome::Skipped => "skipped",
    };
    if materialization == ProjectionMaterialization::InlineCarrier {
        let result_value = serde_json::json!({
            "essential": essential_inline,
            "selection": {
                "mode": selection_facts.mode,
                "origin_call_id": origin_call_id,
                "available_fragments": selection_facts.available_fragments,
                "selected_fragments": selection_facts.selected_fragments,
                "exact_duplicates_removed": selection_facts.exact_duplicates_removed,
                "selected_ids": selection_facts.selected_ids,
                "omitted_inline_ids": selection_facts.omitted_inline_ids,
                "partial_ids": selection_facts.partial_ids,
            },
            "selected_text": "",
            "preserved_content": preserved_content,
            "artifact": null,
        });
        let envelope = ToolProjectionV1 {
            version: 1,
            tool: tool_name,
            outcome: outcome.to_string(),
            canonical_sha256: canonical.sha256,
            canonical_bytes: canonical.exact_bytes,
            canonical_approximate_tokens: canonical.approximate_tokens,
            canonical_complete: canonical.complete,
            model_bytes: 0,
            model_approximate_tokens: 0,
            artifact_id: None,
            sections: Vec::new(),
            omitted_sections: Vec::new(),
            result: result_value,
        };
        let bounded =
            serialize_projection_with_limit(envelope, &projected_text, applied_token_limit)?;
        bounded.envelope()?;
        let rendered = bounded.rendered().to_string();
        return Some(ModelToolProjection {
            original_response,
            bounded,
            passthrough_response: false,
            preserve_non_text_content,
            candidate: None,
            projected_tokens: approx_token_count(&rendered).saturating_add(retained_non_text_tokens)
                as u64,
            canonical_bytes: canonical.exact_bytes,
            canonical_tokens: canonical.approximate_tokens,
            canonical_sha256: canonical_sha256.clone(),
            model_bytes: rendered.len().saturating_add(retained_non_text_bytes) as u64,
            artifact_created: false,
            artifact_reused: false,
            projection_truncated,
            omitted_sections: 0,
            deterministic_continuation_receipt: None,
            deterministic_continuation_content: Vec::new(),
            source_dependencies,
            applied_token_limit: total_applied_token_limit,
        });
    }
    // Admission preserves the producer's packet rather than applying the
    // hypothetical generic projection. Count its actual truncation, including
    // when artifact storage fails, without projecting nested results twice.
    let visible_output_truncated = essential_inline
        .get(crate::tools::code_mode::VISIBLE_OUTPUT_TRUNCATED_KEY)
        == Some(&Value::Bool(true));
    let admission_only_fallback = || {
        let text = original_output_text.replace(
            "artifact_id={cell_output_artifact_id}",
            "saved output unavailable",
        );
        let response = if text != original_output_text {
            projected_response_item(original_response.clone(), text.clone(), true)
        } else {
            original_response.clone()
        };
        let bounded = BoundedModelProjection::Fallback {
            rendered: text.clone(),
        };
        ModelToolProjection {
            original_response: response,
            bounded,
            passthrough_response: true,
            preserve_non_text_content: true,
            candidate: None,
            projected_tokens: approx_token_count(&text).saturating_add(non_text_tokens) as u64,
            canonical_bytes: canonical.exact_bytes,
            canonical_tokens: canonical.approximate_tokens,
            canonical_sha256: canonical_sha256.clone(),
            model_bytes: text.len().saturating_add(non_text_bytes) as u64,
            artifact_created: false,
            artifact_reused: false,
            projection_truncated: visible_output_truncated,
            omitted_sections: 0,
            deterministic_continuation_receipt: None,
            deterministic_continuation_content: Vec::new(),
            source_dependencies: source_dependencies.clone(),
            applied_token_limit: total_applied_token_limit,
        }
    };
    let Some((artifact, artifact_id, artifact_created)) = persisted_artifact else {
        return (materialization == ProjectionMaterialization::AdmissionOnly
            || (essential_inline.get("raw_output_artifact_id").and_then(Value::as_str).is_some()
                && approx_token_count(&original_output_text).saturating_add(non_text_tokens)
                    <= total_applied_token_limit))
            .then(admission_only_fallback);
    };
    let supersession_identity = invocation_sha256.map(|invocation_sha256| {
        format!(
            "authorized-v1:{tool_name}:{invocation_sha256}:{}",
            canonical.sha256
        )
    });
    if materialization == ProjectionMaterialization::AdmissionOnly {
        // Code mode bounds its own packet before admission. When that packet
        // dropped output, name the durable canonical artifact so the omitted
        // range is recoverable without rerunning the producer.
        let (original_response, original_output_text) = if visible_output_truncated {
            let missing_selector = essential_inline.get("cell_output_recovery_selector");
            let pages =
                missing_selector.and_then(|selector| cell_recovery_pages(&canonical, selector));
            let qualified = original_output_text.replace(
                "artifact_id={cell_output_artifact_id}",
                &format!("artifact_id={artifact_id}"),
            );
            let mut notice = serde_json::json!({
                "output_truncated": true,
                "artifact_id": artifact_id,
                "recovery_tool": "read_tool_output",
                "recovery_scope": if missing_selector.is_some() { "omitted_output" } else { "prefix_discovery" },
            });
            match pages {
                Some(CellRecoveryPages {
                    selectors,
                    max_bytes,
                    continuation,
                }) => {
                    let pages = selectors.len();
                    notice["recovery"] = serde_json::json!({"tool":"read_tool_output", "arguments": {
                        "artifact_id":artifact_id,
                        "selectors":selectors,
                        "max_bytes":max_bytes,
                        "cell_recovery":true,
                    }});
                    if pages > 1 || continuation.is_some() {
                        notice["note"] =
                            Value::String(cell_recovery_note(pages, continuation.is_some()));
                    }
                    if let Some(continuation) = continuation {
                        notice["continuation"] = continuation;
                    }
                }
                None => {
                    let selector = missing_selector.cloned().map(|mut selector| {
                        // `ranges` is internal paging input, not a read_tool_output field.
                        if let Some(selector) = selector.as_object_mut() {
                            selector.remove("ranges");
                        }
                        selector
                    }).unwrap_or_else(|| {
                        serde_json::json!({
                            "kind":"bytes", "start":0, "end":canonical.exact_bytes.min(4096)
                        })
                    });
                    notice["recovery"] = serde_json::json!({"tool":"read_tool_output", "arguments": {
                        "artifact_id":artifact_id,
                        "selectors":[selector]
                    }});
                }
            }
            let text = format!("{qualified}\n{notice}");
            (
                projected_response_item(original_response, text.clone(), true),
                text,
            )
        } else {
            (original_response, original_output_text)
        };
        let bounded = BoundedModelProjection::Fallback {
            rendered: original_output_text.clone(),
        };
        return Some(ModelToolProjection {
            original_response,
            bounded,
            passthrough_response: true,
            preserve_non_text_content: true,
            candidate: Some(crate::tool_history::ToolHistoryCandidate {
                call_id: origin_call_id,
                tool_identity: tool_name,
                semantic_class,
                successful,
                source_dependencies: source_dependencies.clone(),
                source_dependencies_current: true,
                artifact_id,
                artifact_bytes: canonical.exact_bytes,
                artifact_sha256: canonical.sha256,
                original_output_sha256,
                original_tokens: original_output_tokens,
                preserved_non_text_tokens: Some(non_text_tokens as u64),
                bounded_model_output: original_output_text.clone(),
                complete: canonical.complete,
                projection_eligible: true,
                proof_identity: None,
                supersession_identity,
                consumed_by_generation: None,
                derived: Default::default(),
            }),
            projected_tokens: approx_token_count(&original_output_text)
                .saturating_add(non_text_tokens) as u64,
            canonical_bytes: canonical.exact_bytes,
            canonical_tokens: canonical.approximate_tokens,
            canonical_sha256: canonical_sha256.clone(),
            model_bytes: original_output_text.len().saturating_add(non_text_bytes) as u64,
            artifact_created,
            artifact_reused: !artifact_created,
            projection_truncated: visible_output_truncated,
            omitted_sections: 0,
            deterministic_continuation_receipt: None,
            deterministic_continuation_content: Vec::new(),
            source_dependencies,
            applied_token_limit: total_applied_token_limit,
        });
    }
    let preserved_content_start = preserved_content.len();
    let result_value = serde_json::json!({
        "essential": essential_inline,
        "selection": {
            "mode": selection_facts.mode,
            "origin_call_id": origin_call_id.clone(),
            "available_fragments": selection_facts.available_fragments,
            "selected_fragments": selection_facts.selected_fragments,
            "exact_duplicates_removed": selection_facts.exact_duplicates_removed,
            "selected_ids": selection_facts.selected_ids,
            "omitted_inline_ids": selection_facts.omitted_inline_ids,
            "partial_ids": selection_facts.partial_ids,
        },
        "selected_text": "",
        "preserved_content": preserved_content,
        "artifact": {
            "retained_bytes": artifact.retained_bytes,
            "complete": artifact.complete,
            "unavailable_ranges": artifact.unavailable_ranges,
            "error": artifact.error,
        }
    });
    let omitted_sections = canonical
        .sections
        .iter()
        .filter(|section| section.inclusion == ToolProjectionInclusion::Omitted)
        .map(|section| section.id.clone())
        .collect::<Vec<_>>();
    let mut envelope = ToolProjectionV1 {
        version: 1,
        tool: tool_name.clone(),
        outcome: outcome.to_string(),
        canonical_sha256: canonical.sha256.clone(),
        canonical_bytes: canonical.exact_bytes,
        canonical_approximate_tokens: canonical.approximate_tokens,
        canonical_complete: canonical.complete,
        model_bytes: 0,
        model_approximate_tokens: 0,
        artifact_id: Some(artifact_id.clone()),
        sections: canonical.sections.clone(),
        omitted_sections,
        result: result_value,
    };
    let fitted =
        serialize_projection_fragments(envelope, &projected_text, &fragments, applied_token_limit)?;
    envelope = fitted.envelope()?.clone();
    let (predetermined_ranges, predetermined_json_pointers) =
        validated_omitted_predetermined_selectors(
            &predetermined_ranges,
            &predetermined_json_pointers,
            &envelope.sections,
            &canonical.json_pointers,
        );
    let (drained_content, mut deterministic_continuation_receipt) =
        drain_predetermined_artifact_selectors(
            &codex_home,
            &thread_id,
            &artifact_id,
            &canonical.sha256,
            predetermined_ranges,
            predetermined_json_pointers,
            &envelope.sections,
            &canonical.json_pointers,
            applied_token_limit,
            Some(&canonical),
        )
        .await;
    let base_envelope = envelope.clone();
    if !drained_content.is_empty() {
        envelope.result["preserved_content"] = Value::Array(
            envelope.result["preserved_content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .chain(drained_content.iter().cloned())
                .collect(),
        );
    }
    // A full untyped fit has not rewritten selection facts or section
    // inclusions. With no recovered content, the second pass has the same
    // rendered header and text budget (rendering recomputes its own metrics).
    // Truncated and typed fits are deliberately excluded: those are not
    // generally idempotent.
    let reuse_first_fit = drained_content.is_empty()
        && fragments.is_empty()
        && envelope.result["selected_text"].as_str() == Some(projected_text.as_str());
    let mut bounded = if reuse_first_fit {
        fitted
    } else {
        serialize_projection_fragments(envelope, &projected_text, &fragments, applied_token_limit)?
    };
    if bounded.envelope().is_none_or(|envelope| {
        !drained_content_survived(envelope, preserved_content_start, &drained_content)
    }) {
        deterministic_continuation_receipt = None;
        bounded = serialize_projection_fragments(
            base_envelope,
            &projected_text,
            &fragments,
            applied_token_limit,
        )?;
    }
    let final_envelope = bounded.envelope()?;
    let omitted_section_count = final_envelope.omitted_sections.len() as u64;
    let projection_truncated = projection_truncated
        || final_envelope.result["selected_text"].as_str() != Some(projected_text.as_str());
    let rendered = bounded.rendered().to_string();
    let projected_tokens =
        approx_token_count(&rendered).saturating_add(retained_non_text_tokens) as u64;
    let model_bytes = rendered.len().saturating_add(retained_non_text_bytes) as u64;
    let deterministic_continuation_content = if deterministic_continuation_receipt.is_some() {
        drained_content
    } else {
        Vec::new()
    };
    Some(ModelToolProjection {
        original_response,
        bounded,
        passthrough_response: false,
        preserve_non_text_content,
        candidate: Some(crate::tool_history::ToolHistoryCandidate {
            call_id: origin_call_id,
            tool_identity: tool_name,
            semantic_class,
            successful,
            source_dependencies: source_dependencies.clone(),
            source_dependencies_current: true,
            artifact_id,
            artifact_bytes: canonical.exact_bytes,
            artifact_sha256: canonical.sha256,
            original_output_sha256,
            original_tokens: original_output_tokens,
            preserved_non_text_tokens: Some(retained_non_text_tokens as u64),
            bounded_model_output: rendered,
            complete: canonical.complete,
            projection_eligible: true,
            proof_identity: None,
            supersession_identity,
            consumed_by_generation: None,
            derived: Default::default(),
        }),
        projected_tokens,
        canonical_bytes: canonical.exact_bytes,
        canonical_tokens: canonical.approximate_tokens,
        canonical_sha256,
        model_bytes,
        artifact_created,
        artifact_reused: !artifact_created,
        projection_truncated,
        omitted_sections: omitted_section_count,
        deterministic_continuation_receipt,
        deterministic_continuation_content,
        source_dependencies,
        applied_token_limit: total_applied_token_limit,
    })
}

fn projected_response_item(
    original: ResponseInputItem,
    rendered: String,
    preserve_non_text_content: bool,
) -> ResponseInputItem {
    match original {
        ResponseInputItem::FunctionCallOutput {
            call_id,
            mut output,
        } => {
            output.body =
                projected_function_output_body(output.body, rendered, preserve_non_text_content);
            ResponseInputItem::FunctionCallOutput { call_id, output }
        }
        ResponseInputItem::CustomToolCallOutput {
            call_id,
            name,
            mut output,
        } => {
            output.body =
                projected_function_output_body(output.body, rendered, preserve_non_text_content);
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                name,
                output,
            }
        }
        ResponseInputItem::McpToolCallOutput {
            call_id,
            mut output,
        } => {
            if preserve_non_text_content {
                output
                    .content
                    .retain(|item| item.get("type").and_then(Value::as_str) != Some("text"));
            } else {
                output.content.clear();
            }
            output.content.insert(
                0,
                serde_json::json!({
                    "type": "text",
                    "text": rendered,
                }),
            );
            output.structured_content = None;
            ResponseInputItem::McpToolCallOutput { call_id, output }
        }
        original => original,
    }
}

fn projected_function_output_body(
    original: FunctionCallOutputBody,
    rendered: String,
    preserve_non_text_content: bool,
) -> FunctionCallOutputBody {
    if !preserve_non_text_content {
        return FunctionCallOutputBody::Text(rendered);
    }
    let FunctionCallOutputBody::ContentItems(items) = original else {
        return FunctionCallOutputBody::Text(rendered);
    };
    FunctionCallOutputBody::ContentItems(
        std::iter::once(FunctionCallOutputContentItem::InputText { text: rendered })
            .chain(
                items.into_iter().filter(|item| {
                    !matches!(item, FunctionCallOutputContentItem::InputText { .. })
                }),
            )
            .collect(),
    )
}

fn validated_predetermined_ranges(
    ranges: &[ToolOutputProjectionRange],
) -> Vec<ToolOutputProjectionRange> {
    if ranges.is_empty() || ranges.len() > 64 {
        return Vec::new();
    }
    let mut normalized = ranges.to_vec();
    normalized.sort_unstable_by_key(|range| (range.start_line, range.end_line, range.id.clone()));
    let mut total_lines = 0_usize;
    let mut prior_end = 0_usize;
    let mut ids = HashSet::new();
    for range in &normalized {
        let Some(lines) = range
            .end_line
            .checked_sub(range.start_line)
            .and_then(|delta| delta.checked_add(1))
        else {
            return Vec::new();
        };
        if range.id.is_empty()
            || range.start_line == 0
            || range.start_line <= prior_end
            || !ids.insert(range.id.clone())
        {
            return Vec::new();
        }
        total_lines = match total_lines.checked_add(lines) {
            Some(total) if total <= 200 => total,
            _ => return Vec::new(),
        };
        prior_end = range.end_line;
    }
    normalized
}

fn validated_omitted_predetermined_ranges(
    ranges: &[ToolOutputProjectionRange],
    sections: &[ToolProjectionSection],
) -> Vec<ToolOutputProjectionRange> {
    let section_by_id = sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<HashMap<_, _>>();
    let candidates = ranges
        .iter()
        .filter(|range| {
            section_by_id
                .get(range.id.as_str())
                .is_some_and(|section| section.inclusion != ToolProjectionInclusion::Included)
        })
        .cloned()
        .collect::<Vec<_>>();
    validated_predetermined_ranges(&candidates)
}

fn validated_predetermined_json_pointers(
    pointers: &[ToolOutputProjectionJsonPointer],
    canonical_json_pointers: &BTreeMap<String, CanonicalJsonPointer>,
) -> Vec<ToolOutputProjectionJsonPointer> {
    if pointers.is_empty() || pointers.len() > 64 {
        return Vec::new();
    }
    let mut normalized = pointers.to_vec();
    normalized.sort_unstable_by(|left, right| {
        left.pointer
            .cmp(&right.pointer)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut ids = HashSet::new();
    let mut selectors = HashSet::new();
    if normalized.iter().any(|selector| {
        selector.id.is_empty()
            || selector.pointer.is_empty()
            || !selector.pointer.starts_with('/')
            || !ids.insert(selector.id.clone())
            || !selectors.insert(selector.pointer.clone())
            || !canonical_json_pointers.contains_key(&selector.pointer)
    }) {
        return Vec::new();
    }
    normalized
}

fn validated_omitted_predetermined_json_pointers(
    pointers: &[ToolOutputProjectionJsonPointer],
    sections: &[ToolProjectionSection],
    canonical_json_pointers: &BTreeMap<String, CanonicalJsonPointer>,
) -> Vec<ToolOutputProjectionJsonPointer> {
    let validated = validated_predetermined_json_pointers(pointers, canonical_json_pointers);
    if !pointers.is_empty() && validated.len() != pointers.len() {
        return Vec::new();
    }
    let section_by_id = sections
        .iter()
        .map(|section| (section.id.as_str(), section))
        .collect::<HashMap<_, _>>();
    let has_section_identity = validated
        .iter()
        .any(|selector| section_by_id.contains_key(selector.id.as_str()));
    if !has_section_identity {
        return validated;
    }
    validated
        .into_iter()
        .filter(|selector| {
            section_by_id
                .get(selector.id.as_str())
                .is_some_and(|section| section.inclusion != ToolProjectionInclusion::Included)
        })
        .collect()
}

fn validated_omitted_predetermined_selectors(
    ranges: &[ToolOutputProjectionRange],
    pointers: &[ToolOutputProjectionJsonPointer],
    sections: &[ToolProjectionSection],
    canonical_json_pointers: &BTreeMap<String, CanonicalJsonPointer>,
) -> (
    Vec<ToolOutputProjectionRange>,
    Vec<ToolOutputProjectionJsonPointer>,
) {
    if ranges.len().saturating_add(pointers.len()) > 64 {
        return (Vec::new(), Vec::new());
    }
    let validated_ranges = validated_predetermined_ranges(ranges);
    if !ranges.is_empty() && validated_ranges.len() != ranges.len() {
        return (Vec::new(), Vec::new());
    }
    let validated_pointers =
        validated_predetermined_json_pointers(pointers, canonical_json_pointers);
    if !pointers.is_empty() && validated_pointers.len() != pointers.len() {
        return (Vec::new(), Vec::new());
    }
    let mut ids = HashSet::new();
    if validated_ranges
        .iter()
        .map(|range| range.id.as_str())
        .chain(
            validated_pointers
                .iter()
                .map(|selector| selector.id.as_str()),
        )
        .any(|id| !ids.insert(id))
    {
        return (Vec::new(), Vec::new());
    }
    (
        validated_omitted_predetermined_ranges(&validated_ranges, sections),
        validated_omitted_predetermined_json_pointers(
            &validated_pointers,
            sections,
            canonical_json_pointers,
        ),
    )
}

// Range and JSON-pointer projections share this private drain boundary but
// retain separate inputs so their persisted continuation contracts stay clear.
#[allow(clippy::too_many_arguments)]
async fn drain_predetermined_artifact_selectors(
    codex_home: &std::path::Path,
    thread_id: &str,
    artifact_id: &str,
    state_revision: &str,
    ranges: Vec<ToolOutputProjectionRange>,
    json_pointers: Vec<ToolOutputProjectionJsonPointer>,
    sections: &[ToolProjectionSection],
    canonical_json_pointers: &BTreeMap<String, CanonicalJsonPointer>,
    token_ceiling: usize,
    producer: Option<&CanonicalToolResult>,
) -> (
    Vec<Value>,
    Option<TurnTimingDeterministicContinuationReceipt>,
) {
    if ranges.is_empty() && json_pointers.is_empty() {
        return (Vec::new(), None);
    }
    let selectors = ranges
        .iter()
        .map(|range| ToolOutputSelector::Lines {
            start: range.start_line,
            end: range.end_line,
        })
        .chain(
            json_pointers
                .iter()
                .map(|selector| ToolOutputSelector::JsonPointer {
                    pointer: selector.pointer.clone(),
                }),
        )
        .collect::<Vec<_>>();
    let selection = if let Some(canonical) = producer
        && canonical.complete
        && canonical.sha256 == state_revision
        && canonical.unavailable_ranges.is_empty()
    {
        crate::tools::command_output_artifact::select_producer_snapshot(
            canonical,
            artifact_id,
            selectors.clone(),
            token_ceiling,
        )
    } else {
        read_tool_output_selectors_with_ceiling_and_reuse(
            codex_home,
            thread_id,
            artifact_id,
            selectors.clone(),
            token_ceiling,
        )
        .await
        .map(|(result, _)| result)
    };
    let Ok(result) = selection else {
        return (Vec::new(), None);
    };
    if !result.complete
        || !result.unavailable_ranges.is_empty()
        || result.results.len() != selectors.len()
        || result.artifact_id != artifact_id
        || result.canonical_sha256 != state_revision
    {
        return (Vec::new(), None);
    }
    let Some(ordered_results) = selectors
        .iter()
        .map(|selector| {
            result
                .results
                .iter()
                .find(|selected| selected.selector == *selector)
                .cloned()
        })
        .collect::<Option<Vec<_>>>()
    else {
        return (Vec::new(), None);
    };

    if ordered_results.iter().enumerate().any(|(index, selected)| {
        selected.status != ToolOutputSelectorStatus::Ok
            || !selected.complete
            || selected.continuation.is_some()
            || if index < ranges.len() {
                selected.text.is_none()
            } else {
                selected.value.is_none()
            }
    }) {
        return (Vec::new(), None);
    }

    if !recovery_matches_section_identity(&ranges, sections, &ordered_results[..ranges.len()])
        || !recovery_matches_json_pointer_identity(
            &json_pointers,
            canonical_json_pointers,
            &ordered_results[ranges.len()..],
        )
    {
        return (Vec::new(), None);
    }

    let action_bounds = serde_json::json!({
        "predetermined_ranges": ranges.iter().map(|range| {
            serde_json::json!({
                "id": range.id,
                "start_line": range.start_line,
                "end_line": range.end_line,
            })
        }).collect::<Vec<_>>(),
        "predetermined_json_pointers": json_pointers,
    });
    let drained = serde_json::json!({
        "type": "deterministic_tool_output_recovery",
        "artifact_id": result.artifact_id,
        "canonical_sha256": result.canonical_sha256,
        "predetermined_ranges": action_bounds["predetermined_ranges"].clone(),
        "predetermined_json_pointers": action_bounds["predetermined_json_pointers"].clone(),
        "results": result.results,
    });
    (
        vec![drained],
        Some(TurnTimingDeterministicContinuationReceipt {
            class: DeterministicContinuationClass::ArtifactRange,
            wire_identity: String::new(),
            resource_identity_hash: crate::tool_history::sha256(artifact_id.as_bytes()),
            state_revision: state_revision.to_string(),
            host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
            action_bounds_hash: crate::tool_history::sha256(
                serde_json::to_string(&action_bounds)
                    .unwrap_or_default()
                    .as_bytes(),
            ),
            suppressed_continuation_count: 1,
        }),
    )
}


fn recovery_matches_section_identity(
    ranges: &[ToolOutputProjectionRange],
    sections: &[ToolProjectionSection],
    results: &[crate::tools::command_output_artifact::ToolOutputSelectorResult],
) -> bool {
    let expected = ranges
        .iter()
        .filter_map(|range| {
            sections
                .iter()
                .find(|section| section.id == range.id)
                .and_then(|section| section.canonical_range)
        })
        .collect::<Vec<_>>();
    if expected.is_empty() {
        return true;
    }
    let actual = results
        .iter()
        .filter_map(|result| result.canonical_range)
        .collect::<Vec<_>>();
    if actual.is_empty() {
        return false;
    }
    expected.iter().all(|expected| {
        let mut covering = actual
            .iter()
            .filter(|actual| actual.start >= expected.start && actual.end <= expected.end);
        let Some(first) = covering.next() else {
            return false;
        };
        if first.start != expected.start {
            return false;
        }
        let mut end = first.end;
        for range in covering {
            if range.start != end {
                return false;
            }
            end = range.end;
        }
        end == expected.end
    })
}

fn recovery_matches_json_pointer_identity(
    pointers: &[ToolOutputProjectionJsonPointer],
    canonical_json_pointers: &BTreeMap<String, CanonicalJsonPointer>,
    results: &[crate::tools::command_output_artifact::ToolOutputSelectorResult],
) -> bool {
    pointers.len() == results.len()
        && pointers.iter().zip(results).all(|(selector, result)| {
            canonical_json_pointers
                .get(&selector.pointer)
                .is_some_and(|entry| result.canonical_range == Some(entry.range))
        })
}

fn drained_content_survived(
    envelope: &ToolProjectionV1,
    preserved_start: usize,
    drained_content: &[Value],
) -> bool {
    if drained_content.is_empty() {
        return true;
    }
    let Some(preserved) = envelope
        .result
        .get("preserved_content")
        .and_then(Value::as_array)
    else {
        return false;
    };
    preserved.get(preserved_start..preserved_start + drained_content.len()) == Some(drained_content)
}

fn serialize_projection_with_limit(
    envelope: ToolProjectionV1,
    output: &str,
    token_limit: usize,
) -> Option<BoundedModelProjection> {
    serialize_projection_fragments(envelope, output, &[], token_limit)
}


fn serialize_projection_fragments(
    mut envelope: ToolProjectionV1,
    output: &str,
    fragments: &[ToolOutputProjectionFragment],
    token_limit: usize,
) -> Option<BoundedModelProjection> {
    // Spillable text remains subject to the requested limit. Structured control
    // state is not spillable, so the smallest typed carrier containing
    // `essential` defines a safe floor when that carrier alone exceeds the
    // request.
    let effective_limit = token_limit.max(1);
    envelope.result["selected_text"] = Value::String(String::new());
    // The recovery directory can grow when fitting marks sections omitted.
    // Reserve it up front, so payload selection never truncates an earlier cut.
    let mut header = envelope.clone();
    header.omitted_sections = header
        .sections
        .iter()
        .map(|section| section.id.clone())
        .collect();
    let header_tokens = approx_token_count(&render_projection_with_exact_metrics(&mut header)?);
    let mut output_limit = effective_limit.saturating_sub(header_tokens.saturating_add(1));
    loop {
        let retained = if fragments.is_empty() {
            codex_utils_output_truncation::truncate_text_with_line_markers(output, output_limit)
        } else {
            let (text, selection) = select_typed_projection_fragments(fragments, output_limit);
            for section in &mut envelope.sections {
                section.inclusion = if selection.selected_ids.contains(&section.id)
                    && !selection.partial_ids.contains(&section.id)
                {
                    ToolProjectionInclusion::Included
                } else {
                    ToolProjectionInclusion::Omitted
                };
            }
            if let Some(facts) = envelope.result.get_mut("selection") {
                facts["mode"] = selection.mode.into();
                facts["available_fragments"] = selection.available_fragments.into();
                facts["selected_fragments"] = selection.selected_fragments.into();
                facts["exact_duplicates_removed"] = selection.exact_duplicates_removed.into();
                facts["selected_ids"] = serde_json::json!(selection.selected_ids);
                facts["partial_ids"] = serde_json::json!(selection.partial_ids);
                facts["omitted_inline_ids"] = serde_json::json!(selection.omitted_inline_ids);
            }
            text
        };
        if retained != output {
            for section in &mut envelope.sections {
                if fragments.is_empty() && section.inclusion == ToolProjectionInclusion::Included {
                    section.inclusion = ToolProjectionInclusion::Omitted;
                }
            }
            envelope.omitted_sections = envelope
                .sections
                .iter()
                .filter(|section| section.inclusion == ToolProjectionInclusion::Omitted)
                .map(|section| section.id.clone())
                .collect();
            if fragments.is_empty()
                && let Some(selection) = envelope.result.get_mut("selection")
            {
                let selected = selection["selected_ids"].clone();
                selection["partial_ids"] = selected.clone();
                if retained.is_empty() {
                    let mut omitted = selection["omitted_inline_ids"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    omitted.extend(selected.as_array().cloned().unwrap_or_default());
                    selection["omitted_inline_ids"] = omitted.into();
                    selection["selected_ids"] = serde_json::json!([]);
                    selection["partial_ids"] = serde_json::json!([]);
                    selection["selected_fragments"] = 0.into();
                }
            }
        }
        envelope.omitted_sections = envelope
            .sections
            .iter()
            .filter(|section| section.inclusion == ToolProjectionInclusion::Omitted)
            .map(|section| section.id.clone())
            .collect();
        envelope.result["selected_text"] = Value::String(retained);
        let rendered = render_projection_with_exact_metrics(&mut envelope)?;
        let rendered_tokens = approx_token_count(&rendered);
        if rendered_tokens <= effective_limit {
            return Some(BoundedModelProjection::Envelope { envelope, rendered });
        }
        if output_limit == 0 {
            break;
        }
        output_limit = output_limit
            .saturating_sub((rendered_tokens - effective_limit).max(1))
            // Token estimates need not be monotonic across truncation markers
            // or UTF-8 boundaries. Bound retries geometrically, checking every
            // rendered candidate instead of assuming binary-search ordering.
            .min(output_limit - output_limit.div_ceil(4));
    }

    let essential = envelope
        .result
        .get("essential")
        .cloned()
        .unwrap_or(Value::Null);
    envelope.sections.clear();
    envelope.omitted_sections.clear();
    envelope.result = serde_json::json!({
        "essential": essential,
        "selected_text": "",
    });
    let rendered = render_projection_with_exact_metrics(&mut envelope)?;
    Some(BoundedModelProjection::Envelope { envelope, rendered })
}

fn render_projection_with_exact_metrics(envelope: &mut ToolProjectionV1) -> Option<String> {
    let selected_text = envelope.result["selected_text"]
        .as_str()
        .unwrap_or_default();
    let mut fields = envelope.result["essential"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    // Remove transport bookkeeping, not typed outcome or completeness. Tiny
    // budgets can remove every diagnostic line, leaving this as the only carrier.
    for key in [
        "chunk_id",
        "wall_time_seconds",
        "original_token_count",
        "original_token_count_is_approximate",
        "success",
        "raw_output_artifact_bytes",
        "raw_output_artifact_id",
        "changes_count",
        "environment_id",
        "nested_commands",
    ] {
        fields.remove(key);
    }
    fields.retain(|_, value| !value.is_null());
    let outcome_visible = fields.get("outcome").and_then(Value::as_str)
        == Some(envelope.outcome.as_str())
        || (envelope.outcome == "success"
            && fields.get("exit_code").and_then(Value::as_i64) == Some(0))
        || (envelope.outcome == "failure"
            && fields
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0));
    if !outcome_visible {
        fields.insert("outcome".into(), envelope.outcome.clone().into());
    }
    if !envelope.canonical_complete {
        fields.insert("canonical_complete".into(), false.into());
    }
    if selected_text.len() < envelope.canonical_bytes as usize {
        fields.insert("output_reduced".into(), true.into());
    }
    if let Some(artifact_id) = envelope.artifact_id.as_ref()
        && (!envelope.omitted_sections.is_empty()
            || selected_text.is_empty()
            || envelope.canonical_bytes > selected_text.len() as u64)
    {
        fields.insert(
            "artifact_id".to_string(),
            Value::String(artifact_id.clone()),
        );
        if !fields.contains_key("recovery") && envelope.canonical_bytes > 0 {
            // Section ranges are authoritative canonical addresses. A partial
            // section may include already shown bytes; do not present its full
            // range as an exact gap or infer coordinates from rendered markers.
            let mut selectors = envelope
                .sections
                .iter()
                .filter(|section| section.inclusion == ToolProjectionInclusion::Omitted)
                .filter_map(|section| section.canonical_range)
                .filter(|range| range.start < range.end && range.end <= envelope.canonical_bytes)
                .map(|range| serde_json::json!({"kind":"bytes", "start":range.start, "end":range.end}))
                .collect::<Vec<_>>();
            let scope = if selectors.is_empty() || selectors.len() > 64 {
                selectors = vec![serde_json::json!({"kind":"bytes", "start":0, "end":envelope.canonical_bytes})];
                "canonical_result"
            } else {
                "omitted_sections"
            };
            fields.insert("recovery_scope".into(), scope.into());
            fields.insert("recovery".into(), serde_json::json!({
                "tool":"read_tool_output", "arguments": {
                    "artifact_id":artifact_id, "selectors":selectors
                }
            }));
        }
        if !envelope.omitted_sections.is_empty() {
            fields.insert(
                "omitted_sections".to_string(),
                serde_json::json!(envelope.omitted_sections),
            );
        }
    }
    let mut parts = Vec::new();
    if !fields.is_empty() {
        parts.push(serde_json::to_string(&fields).ok()?);
    }
    if !selected_text.is_empty() {
        parts.push(selected_text.to_string());
    }
    if let Some(preserved) = envelope.result["preserved_content"].as_array() {
        for value in preserved {
            parts.push(
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string()),
            );
        }
    }
    let rendered = parts.join("\n");
    envelope.model_bytes = rendered.len() as u64;
    envelope.model_approximate_tokens = approx_token_count(&rendered) as u64;
    Some(rendered)
}

pub(super) fn preserved_non_text_content(response: &ResponseInputItem) -> Vec<Value> {
    let output = match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output,
        ResponseInputItem::McpToolCallOutput { output, .. } => {
            return output
                .content
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) != Some("text"))
                .cloned()
                .collect();
        }
        _ => return Vec::new(),
    };
    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| !matches!(item, FunctionCallOutputContentItem::InputText { .. }))
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect()
}

fn non_text_projection_token_cost(content: &[Value]) -> usize {
    // Typed images are charged and validated by image_preparation, not by
    // serializing their base64 as text. MCP's native content remains separate.
    let text_budgeted = content
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("input_image"))
        .collect::<Vec<_>>();
    if text_budgeted.is_empty() {
        return 0;
    }
    serialized_json_byte_cost(&text_budgeted)
        .map(|bytes| codex_utils_string::TokenCountEstimate::from_byte_count(bytes).tokens())
        .unwrap_or(usize::MAX)
}

fn non_text_projection_byte_cost(content: &[Value]) -> usize {
    if content.is_empty() {
        return 0;
    }
    serialized_json_byte_cost(content).unwrap_or(usize::MAX)
}

fn serialized_json_byte_cost(value: &(impl serde::Serialize + ?Sized)) -> Result<usize, serde_json::Error> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("serialized byte count overflowed"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}
