use crate::FunctionCallError;
use crate::agent::task_capabilities::TypedToolClass;
use crate::client_common::ToolSchemaArtifact;
use crate::hook_runtime::PreToolUseHookResult;
use crate::hook_runtime::prepare_additional_context_items;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::run_post_tool_use_hooks;
use crate::hook_runtime::run_pre_tool_use_hooks;
use crate::memory_usage::emit_metric_for_tool_read;
use crate::sandbox_tags::permission_profile_policy_tag;
use crate::sandbox_tags::permission_profile_sandbox_tag;
use crate::session::reasoning_governor::PendingOwnerDrainedContinuation;
use crate::session::turn_context::TurnContext;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::attach_canonical_output_artifact;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_ceiling_and_reuse;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::flat_tool_name;
use crate::tools::handlers::ParsedFunctionArguments;
use crate::tools::handlers::multi_agents_spec::MULTI_AGENT_V1_NAMESPACE;
use crate::tools::handlers::parsed_function_argument_value;
use crate::tools::handlers::with_parsed_function_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::lifecycle::notify_tool_finish;
use crate::tools::lifecycle::notify_tool_start;
use crate::tools::tool_dispatch_trace::ToolDispatchTrace;
use crate::tools::tool_dispatch_trace::mark_tool_handler_entry;
use crate::tools::tool_dispatch_trace::mark_tool_handler_exit;
use crate::tools::tool_dispatch_trace::record_history_persistence;
use crate::tools::tool_dispatch_trace::record_output_projection;
use crate::tools::tool_dispatch_trace::record_post_tool_hook;
use crate::tools::tool_dispatch_trace::record_pre_tool_hook;
use crate::turn_timing::TurnTimingState;
use codex_config::schema::canonicalize as canonicalize_json;
use codex_extension_api::ToolCallOutcome;
use codex_hooks::PostToolUseOutcome;
use codex_otel::TOOL_LIFECYCLE_PHASE_DURATION_METRIC;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_rollout::state_integration;
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
use codex_tools::ToolSpec;
use codex_utils_output_truncation::DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS;
use codex_utils_output_truncation::OutputDiagnosticClass;
use codex_utils_output_truncation::OutputOutcome;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::formatted_truncate_text_with_output_limit;
use codex_utils_output_truncation::resolve_projected_output_limits;
use codex_utils_output_truncation::truncate_text_to_token_ceiling;
use futures::future::BoxFuture;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

pub(crate) type ToolTelemetryTags = Vec<(&'static str, String)>;
const MIN_PROJECTION_ENVELOPE_TOKENS: usize = 64;

fn record_lifecycle_phase(invocation: &ToolInvocation, phase: &'static str, started: Instant) {
    let tool_name = flat_tool_name(&invocation.tool_name);
    let elapsed = started.elapsed();
    invocation
        .step_context
        .turn
        .session_telemetry
        .record_duration(
            TOOL_LIFECYCLE_PHASE_DURATION_METRIC,
            elapsed,
            &[("phase", phase), ("tool", tool_name.as_ref())],
        );
    match phase {
        "pre_hooks" => record_pre_tool_hook(elapsed),
        "post_hooks" => record_post_tool_hook(elapsed),
        "projection" => record_output_projection(elapsed),
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolExecutionTiming {
    /// Time the core handler future as the actual tool invocation.
    Handler,
    /// A narrower runtime boundary inside the handler owns execution timing.
    NestedRuntime,
    /// The handler is an interactive wait rather than machine tool execution.
    Interactive,
}

pub use codex_tools::ToolExecutor;
pub use codex_tools::ToolExposure;

/// Typed runtime contract for locally executed tools.
///
/// Implementers provide the shared `ToolExecutor` behavior plus optional
/// core-owned metadata for hooks, telemetry, tool search, and argument diffs.
pub(crate) trait CoreToolRuntime: ToolExecutor<ToolInvocation> {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::Handler
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            payload,
            ToolPayload::Function { .. } | ToolPayload::ToolSearch { .. }
        )
    }

    /// Whether cancellation should let the handler finish teardown before the
    /// host returns an aborted tool response.
    fn waits_for_runtime_cancellation(&self) -> bool {
        false
    }

    /// Whether this runtime can retain processes in the unified-exec manager.
    /// Routing cleanup by runtime capability keeps aliases and namespaced
    /// registrations on the same cancellation path as the canonical name.
    fn owns_unified_exec_processes(&self) -> bool {
        false
    }

    fn telemetry_tags<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
    ) -> BoxFuture<'a, ToolTelemetryTags> {
        Box::pin(async { Vec::new() })
    }

    fn post_tool_use_hook_name(&self, invocation: &ToolInvocation) -> Option<HookToolName> {
        matches!(&invocation.payload, ToolPayload::Function { .. })
            .then(|| function_hook_tool_name(invocation))
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PostToolUsePayload {
            tool_name: self.post_tool_use_hook_name(invocation)?,
            tool_use_id: result.post_tool_use_id(&invocation.call_id),
            tool_input: result
                .post_tool_use_input(&invocation.payload)
                .unwrap_or_else(|| function_hook_tool_input(arguments)),
            tool_response: result
                .post_tool_use_response(&invocation.call_id, &invocation.payload)
                .or_else(|| {
                    // Most function tools can expose their model-facing output
                    // as the hook response. Outputs with a more stable hook
                    // contract should override post_tool_use_response above.
                    let ResponseInputItem::FunctionCallOutput {
                        output: FunctionCallOutputPayload { body, .. },
                        ..
                    } = result.to_response_item(&invocation.call_id, &invocation.payload)
                    else {
                        return None;
                    };

                    serde_json::to_value(body).ok()
                })?,
        })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PreToolUsePayload {
            tool_name: function_hook_tool_name(invocation),
            tool_input: function_hook_tool_input(arguments),
        })
    }

    /// Rebuilds a tool invocation from hook-facing `tool_input`.
    ///
    /// Tools that opt into input-rewriting hooks should invert the same stable
    /// hook contract they expose from `pre_tool_use_payload`.
    fn with_updated_hook_input(
        &self,
        invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { .. } = &invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported function tool payload".to_string(),
            ));
        };

        let arguments = serde_json::to_string(&updated_input).map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to serialize rewritten {} arguments: {err}",
                flat_tool_name(&invocation.tool_name)
            ))
        })?;
        Ok(ToolInvocation {
            payload: ToolPayload::Function { arguments },
            ..invocation
        })
    }

    /// Creates an optional consumer for streamed tool argument diffs.
    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        None
    }
}

/// Consumes streamed argument diffs for a tool call and emits protocol events
/// derived from partial tool input.
pub(crate) trait ToolArgumentDiffConsumer: Send {
    /// Consume the next argument diff for a tool call.
    fn consume_diff(&mut self, turn: &TurnContext, call_id: String, diff: &str)
    -> Option<EventMsg>;

    /// Finish consuming argument diffs before the tool call completes.
    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        Ok(None)
    }
}

pub(crate) struct AnyToolResult {
    pub(crate) call_id: String,
    pub(crate) payload: ToolPayload,
    pub(crate) result: Box<dyn ToolOutput>,
    pub(crate) model_projection: Option<ModelToolProjection>,
    pub(crate) source_dependencies:
        Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>>,
    pub(crate) code_mode_feedback: Vec<FunctionCallOutputContentItem>,
}

pub(crate) struct ModelToolProjection {
    original_response: ResponseInputItem,
    bounded: BoundedModelProjection,
    passthrough_response: bool,
    preserve_non_text_content: bool,
    candidate: Option<crate::tool_history::ToolHistoryCandidate>,
    projected_tokens: u64,
    canonical_bytes: u64,
    canonical_tokens: u64,
    model_bytes: u64,
    artifact_created: bool,
    artifact_reused: bool,
    projection_truncated: bool,
    omitted_sections: u64,
    deterministic_continuation_receipt: Option<TurnTimingDeterministicContinuationReceipt>,
    deterministic_continuation_content: Vec<Value>,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    applied_token_limit: usize,
}

#[derive(Clone, Debug)]
enum BoundedModelProjection {
    Envelope {
        envelope: ToolProjectionV1,
        rendered: String,
    },
    Fallback {
        value: Value,
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

    #[cfg(test)]
    fn value(&self) -> Value {
        match self {
            Self::Envelope { envelope, .. } => {
                serde_json::to_value(envelope).unwrap_or(Value::Null)
            }
            Self::Fallback { value, .. } => value.clone(),
        }
    }

    fn rendered(&self) -> &str {
        match self {
            Self::Envelope { rendered, .. } | Self::Fallback { rendered, .. } => rendered,
        }
    }

    fn into_rendered(self) -> String {
        match self {
            Self::Envelope { rendered, .. } => rendered,
            Self::Fallback { value, rendered } => {
                debug_assert_eq!(
                    serde_json::to_string(&value).ok().as_deref(),
                    Some(rendered.as_str()),
                );
                rendered
            }
        }
    }

    fn into_value(self) -> Value {
        match self {
            Self::Envelope { envelope, .. } => {
                serde_json::to_value(envelope).unwrap_or(Value::Null)
            }
            Self::Fallback { value, .. } => value,
        }
    }
}

impl AnyToolResult {
    fn install_model_projection(
        &mut self,
        model_projection: Option<ModelToolProjection>,
        source_dependencies_override: Option<
            std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        >,
    ) {
        self.source_dependencies = source_dependencies_override
            .or_else(|| {
                model_projection
                    .as_ref()
                    .map(|projection| projection.source_dependencies.clone())
            })
            .or_else(precomputed_projection_source_dependencies);
        self.model_projection = model_projection;
    }

    pub(crate) fn response(&self) -> ResponseInputItem {
        if let Some(projection) = self.model_projection.as_ref() {
            return projection.response();
        }
        self.result.to_response_item(&self.call_id, &self.payload)
    }

    pub(crate) fn success_for_logging(&self) -> bool {
        self.result.success_for_logging()
    }

    pub(crate) fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.result.outcome_for_logging()
    }

    pub(crate) fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        self.result.outcome_context()
    }

    pub(crate) fn sampling_request_signal(&self) -> Option<serde_json::Value> {
        self.result.sampling_request_signal()
    }

    pub(crate) fn requires_canonical_artifact(&self) -> bool {
        self.result.requires_canonical_artifact()
    }

    pub(crate) fn projected_source_dependencies(
        &self,
    ) -> Option<&std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>> {
        self.source_dependencies.as_ref()
    }

    pub(crate) fn take_code_mode_feedback(&mut self) -> Vec<FunctionCallOutputContentItem> {
        std::mem::take(&mut self.code_mode_feedback)
    }

    pub(crate) fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        let mut receipts = self.result.deterministic_continuation_receipts();
        if let Some(receipt) = self
            .model_projection
            .as_ref()
            .and_then(|projection| projection.deterministic_continuation_receipt.clone())
        {
            receipts.push(receipt);
        }
        receipts
    }

    pub(crate) fn intrinsic_deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        let owner_drained_identity = self
            .owner_drained_continuation()
            .and_then(|continuation| continuation.receipt.runtime_identity());
        self.result
            .deterministic_continuation_receipts()
            .into_iter()
            .filter(|receipt| {
                owner_drained_identity
                    .as_ref()
                    .is_none_or(|identity| receipt.runtime_identity().as_ref() != Some(identity))
            })
            .collect()
    }

    pub(crate) fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.result.deterministic_continuation_owner_key()
    }

    pub(crate) fn owner_drained_continuation(&self) -> Option<PendingOwnerDrainedContinuation> {
        let result_content = self.result.deterministic_continuation_content();
        if !result_content.is_empty() {
            let mut receipts = self
                .result
                .deterministic_continuation_receipts()
                .into_iter();
            let receipt = receipts.next()?;
            if receipts.next().is_none() && valid_owner_drained_receipt(&receipt) {
                return Some(PendingOwnerDrainedContinuation {
                    preserved_content: result_content,
                    receipt,
                });
            }
        }
        let projection = self.model_projection.as_ref()?;
        let receipt = projection.deterministic_continuation_receipt.clone()?;
        if !valid_owner_drained_receipt(&receipt) {
            return None;
        }
        Some(PendingOwnerDrainedContinuation {
            preserved_content: (!projection.deterministic_continuation_content.is_empty())
                .then(|| projection.deterministic_continuation_content.clone())?,
            receipt,
        })
    }

    pub(crate) fn merge_owner_drained_continuations(
        &mut self,
        continuations: Vec<PendingOwnerDrainedContinuation>,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.model_projection
            .as_mut()
            .map_or_else(Vec::new, |projection| {
                projection.merge_owner_drained_continuations(continuations)
            })
    }

    pub(crate) fn into_response(self) -> ResponseInputItem {
        if let Some(projection) = self.model_projection {
            return projection.into_response();
        }
        let Self {
            call_id,
            payload,
            result,
            ..
        } = self;
        result.to_response_item(&call_id, &payload)
    }

    pub(crate) fn code_mode_result(self) -> serde_json::Value {
        let Self {
            payload,
            result,
            model_projection,
            ..
        } = self;
        match model_projection {
            Some(projection) => projection.into_code_mode_result(),
            None => result.code_mode_result(&payload),
        }
    }
}

/// Materialize the model-visible representation for a terminal result owned by
/// the cancellation path. That path deliberately bypasses post-tool hooks and
/// the ordinary registry finish notification, but it still owes the same
/// projection attribution as every other accepted tool call.
pub(crate) async fn install_synthetic_terminal_projection(
    invocation: &ToolInvocation,
    result: &mut AnyToolResult,
) {
    let parsed_function_arguments = ParsedFunctionArguments::from_payload(&invocation.payload);
    let provider_visible = projection_is_provider_visible(&invocation.source);
    let phase_started = Instant::now();
    let model_projection = match prepare_model_projection(
        invocation,
        result,
        parsed_function_arguments.as_ref(),
        /*source_dependencies_override*/ None,
        /*force_inline_carrier*/ false,
        /*track_for_admission*/ false,
    ) {
        Some(input) => project_model_output(input).await,
        None => None,
    };
    record_lifecycle_phase(invocation, "projection", phase_started);
    if let Some(projection) = &model_projection {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_tool_output_projection_facts(
                projection.canonical_bytes,
                projection.canonical_tokens,
                projection.model_bytes,
                projection.projected_tokens,
                projection.artifact_created,
                projection.artifact_reused,
                projection.projection_truncated,
                projection.omitted_sections,
                provider_visible,
            );
    }
    result.install_model_projection(model_projection, /*source_dependencies_override*/ None);
}

impl ModelToolProjection {
    fn response(&self) -> ResponseInputItem {
        if self.passthrough_response {
            return self.original_response.clone();
        }
        projected_response_item(
            self.original_response.clone(),
            self.bounded.rendered().to_string(),
            self.preserve_non_text_content,
        )
    }

    fn into_response(self) -> ResponseInputItem {
        if self.passthrough_response {
            return self.original_response;
        }
        projected_response_item(
            self.original_response,
            self.bounded.into_rendered(),
            self.preserve_non_text_content,
        )
    }

    fn into_code_mode_result(self) -> Value {
        self.bounded.into_value()
    }

    fn merge_owner_drained_continuations(
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
            let accepted_candidate =
                [projected_text.as_str(), ""]
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

fn valid_owner_drained_receipt(receipt: &TurnTimingDeterministicContinuationReceipt) -> bool {
    !receipt.resource_identity_hash.is_empty()
        && !receipt.state_revision.is_empty()
        && receipt.runtime_identity().is_some()
        && receipt.suppressed_continuation_count > 0
}

struct PostToolUseFeedbackOutput {
    original: Box<dyn ToolOutput>,
    model_visible: FunctionToolOutput,
}

fn apply_post_tool_use_outcome(
    result: &mut AnyToolResult,
    outcome: PostToolUseOutcome,
) -> Vec<String> {
    // A post hook observes a call whose side effects have already happened. Its
    // block decision cannot undo the call, so returning a failed tool result
    // would only invite the model to repeat the mutation. Likewise, context
    // from a rejecting hook must not influence the next sampling request.
    if outcome.should_block {
        return Vec::new();
    }
    if let Some(feedback_message) = outcome.feedback_message {
        let model_visible = FunctionToolOutput::from_text(feedback_message, /*success*/ None);
        result.code_mode_feedback = model_visible.body.clone();
        result.result = Box::new(PostToolUseFeedbackOutput {
            original: std::mem::replace(
                &mut result.result,
                Box::new(FunctionToolOutput::from_text(String::new(), None)),
            ),
            model_visible,
        });
    }
    outcome.additional_contexts
}

impl ToolOutput for PostToolUseFeedbackOutput {
    fn log_preview(&self) -> String {
        self.original.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.original.success_for_logging()
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        self.original.outcome_for_logging()
    }

    fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        self.original.outcome_context()
    }

    fn requires_canonical_artifact(&self) -> bool {
        self.original.requires_canonical_artifact()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        self.original.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.original.deterministic_continuation_receipts()
    }

    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.original.deterministic_continuation_owner_key()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.original.deterministic_continuation_content()
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        let mut metadata = self.model_visible.projection_metadata()?;
        if let Some(original) = self.original.projection_metadata() {
            metadata.merge_essential_fields(original.essential_inline);
        }
        Some(metadata)
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.original.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.model_visible.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.original.code_mode_result(payload)
    }
}

struct UnavailableModelProjectionOutput {
    original: Box<dyn ToolOutput>,
    model_visible: FunctionToolOutput,
}

impl ToolOutput for UnavailableModelProjectionOutput {
    fn log_preview(&self) -> String {
        self.original.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        false
    }

    fn outcome_for_logging(&self) -> ToolOutputOutcome {
        ToolOutputOutcome::Failure
    }

    fn outcome_context(&self) -> codex_tools::ToolOutputOutcomeContext {
        self.original.outcome_context()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        self.original.sampling_request_signal()
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.original.deterministic_continuation_receipts()
    }

    fn deterministic_continuation_owner_key(&self) -> Option<String> {
        self.original.deterministic_continuation_owner_key()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.original.deterministic_continuation_content()
    }

    fn contains_external_context(&self) -> bool {
        self.original.contains_external_context()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        self.model_visible.projection_metadata()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.model_visible.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.model_visible.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.model_visible.code_mode_result(payload)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreToolUsePayload {
    /// Hook-facing tool name model.
    ///
    /// The canonical name is serialized to hook stdin, while aliases are used
    /// only for matcher compatibility.
    pub(crate) tool_name: HookToolName,
    /// Tool-specific input exposed at `tool_input`.
    ///
    /// Shell-like tools use `{ "command": ... }`; MCP tools use their resolved
    /// JSON arguments.
    pub(crate) tool_input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PostToolUsePayload {
    /// Hook-facing tool name model.
    ///
    /// The canonical name is serialized to hook stdin, while aliases are used
    /// only for matcher compatibility.
    pub(crate) tool_name: HookToolName,
    /// The originating tool-use id exposed at `tool_use_id`.
    pub(crate) tool_use_id: String,
    /// Tool-specific input exposed at `tool_input`.
    pub(crate) tool_input: Value,
    /// Tool result exposed at `tool_response`.
    pub(crate) tool_response: Value,
}

pub(crate) struct RegisteredTool {
    tool_name: ToolName,
    runtime: Arc<dyn CoreToolRuntime>,
    exposure: ToolExposure,
    authorization_class: TypedToolClass,
    external_mutation_intent: crate::agent::task_capabilities::ExternalMutationIntent,
    spec: ToolSpec,
    canonical_spec_sha256: OnceLock<String>,
    code_mode_argument_preflight: Arc<CodeModeArgumentPreflight>,
}

impl RegisteredTool {
    pub(crate) fn new(
        runtime: Arc<dyn CoreToolRuntime>,
        authorization_class: TypedToolClass,
    ) -> Self {
        assert_ne!(
            authorization_class,
            TypedToolClass::Unknown,
            "registered tools must declare an authorization class"
        );
        let exposure = runtime.exposure();
        let spec = runtime.spec();
        let tool_name = spec.sole_callable_tool_name().unwrap_or_else(|| {
            panic!("registered runtime specs must declare exactly one callable tool")
        });
        Self {
            tool_name,
            runtime,
            exposure,
            authorization_class,
            external_mutation_intent:
                crate::agent::task_capabilities::ExternalMutationIntent::MayMutate,
            spec,
            canonical_spec_sha256: OnceLock::new(),
            code_mode_argument_preflight: Arc::new(CodeModeArgumentPreflight::default()),
        }
    }

    pub(crate) fn with_exposure(
        runtime: Arc<dyn CoreToolRuntime>,
        exposure: ToolExposure,
        authorization_class: TypedToolClass,
    ) -> Self {
        assert_ne!(
            authorization_class,
            TypedToolClass::Unknown,
            "registered tools must declare an authorization class"
        );
        let spec = runtime.spec();
        let tool_name = spec.sole_callable_tool_name().unwrap_or_else(|| {
            panic!("registered runtime specs must declare exactly one callable tool")
        });
        Self {
            tool_name,
            runtime,
            exposure,
            authorization_class,
            external_mutation_intent:
                crate::agent::task_capabilities::ExternalMutationIntent::MayMutate,
            spec,
            canonical_spec_sha256: OnceLock::new(),
            code_mode_argument_preflight: Arc::new(CodeModeArgumentPreflight::default()),
        }
    }

    pub(crate) fn with_external_mutation_intent(
        mut self,
        intent: crate::agent::task_capabilities::ExternalMutationIntent,
    ) -> Self {
        self.external_mutation_intent = intent;
        self
    }

    pub(crate) fn runtime(&self) -> &Arc<dyn CoreToolRuntime> {
        &self.runtime
    }

    pub(crate) fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub(crate) fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    pub(crate) fn authorization_class(&self) -> TypedToolClass {
        self.authorization_class
    }

    pub(crate) fn external_mutation_intent(
        &self,
    ) -> crate::agent::task_capabilities::ExternalMutationIntent {
        self.external_mutation_intent
    }

    pub(crate) fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    pub(crate) fn spec_mut(&mut self) -> &mut ToolSpec {
        self.canonical_spec_sha256.take();
        &mut self.spec
    }

    pub(crate) fn canonical_spec_sha256(&self) -> &str {
        self.canonical_spec_sha256.get_or_init(|| {
            let canonical_spec =
                canonicalize_json(&serde_json::to_value(&self.spec).unwrap_or_default());
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&canonical_spec).unwrap_or_default())
            )
        })
    }

    pub(crate) fn search_info(&self) -> Option<codex_tools::ToolSearchInfo> {
        self.runtime.search_info_for_registered_spec(&self.spec)
    }

    pub(crate) fn set_exposure(&mut self, exposure: ToolExposure) {
        self.exposure = exposure;
    }
}

pub struct ToolRegistry {
    tools: HashMap<ToolName, RegisteredTool>,
    model_visible_schemas: Arc<ToolSchemaArtifact>,
}

impl ToolRegistry {
    fn new(tools: HashMap<ToolName, RegisteredTool>) -> Self {
        Self {
            tools,
            model_visible_schemas: Arc::new(ToolSchemaArtifact::default()),
        }
    }

    #[cfg(test)]
    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) fn from_tools(tools: impl IntoIterator<Item = Arc<dyn CoreToolRuntime>>) -> Self {
        Self::from_unique_registered_tools(
            tools
                .into_iter()
                .map(|runtime| RegisteredTool::new(runtime, TypedToolClass::DynamicExternal)),
        )
    }

    pub(crate) fn from_unique_registered_tools(
        tools: impl IntoIterator<Item = RegisteredTool>,
    ) -> Self {
        let tools_by_name = tools
            .into_iter()
            .map(|tool| (tool.tool_name().clone(), tool))
            .collect();
        Self::new(tools_by_name)
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self::new(HashMap::new())
    }

    #[cfg(test)]
    pub(crate) fn with_handler_for_test<T>(handler: Arc<T>) -> Self
    where
        T: CoreToolRuntime + 'static,
    {
        let runtime = handler as Arc<dyn CoreToolRuntime>;
        let registered = RegisteredTool::new(runtime, TypedToolClass::DynamicExternal);
        Self::new(HashMap::from([(
            registered.tool_name().clone(),
            registered,
        )]))
    }

    fn tool(&self, name: &ToolName) -> Option<Arc<dyn CoreToolRuntime>> {
        self.tools.get(name).map(|tool| Arc::clone(tool.runtime()))
    }

    #[cfg(test)]
    pub(crate) fn tool_names_for_test(&self) -> Vec<ToolName> {
        let mut names = self.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    pub(crate) fn tool_exposure(&self, name: &ToolName) -> Option<ToolExposure> {
        self.tools.get(name).map(RegisteredTool::exposure)
    }

    pub(crate) fn tool_authorization_class(&self, name: &ToolName) -> Option<TypedToolClass> {
        self.tools
            .get(name)
            .map(RegisteredTool::authorization_class)
    }

    pub(crate) fn tool_external_mutation_intent(
        &self,
        name: &ToolName,
    ) -> Option<crate::agent::task_capabilities::ExternalMutationIntent> {
        self.tools
            .get(name)
            .map(RegisteredTool::external_mutation_intent)
    }

    pub(crate) fn set_model_visible_specs(&mut self, specs: Vec<ToolSpec>) {
        self.model_visible_schemas = Arc::new(ToolSchemaArtifact::new(specs));
    }

    pub(crate) fn model_visible_specs(&self) -> Vec<ToolSpec> {
        self.model_visible_schemas.specs().to_vec()
    }

    pub(crate) fn model_visible_schemas(&self) -> Arc<ToolSchemaArtifact> {
        Arc::clone(&self.model_visible_schemas)
    }

    pub(crate) fn manifest_entries(&self) -> Vec<&RegisteredTool> {
        let mut entries = self.tools.values().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.tool_name().cmp(right.tool_name()));
        entries
    }

    pub(crate) fn create_diff_consumer(
        &self,
        name: &ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.tool(name)?.create_diff_consumer()
    }

    pub(crate) fn supports_parallel_tool_calls(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tools.get(name)?;
        Some(
            tool.exposure() != ToolExposure::Hidden
                && tool.runtime().supports_parallel_tool_calls(),
        )
    }

    pub(crate) fn waits_for_runtime_cancellation(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.waits_for_runtime_cancellation())
    }

    pub(crate) fn owns_unified_exec_processes(&self, name: &ToolName) -> Option<bool> {
        let tool = self.tool(name)?;
        Some(tool.owns_unified_exec_processes())
    }

    #[cfg(test)]
    pub(crate) fn code_mode_argument_preflight_counts(
        &self,
        name: &ToolName,
    ) -> Option<(usize, usize)> {
        let preflight = &self.tools.get(name)?.code_mode_argument_preflight;
        Some((
            preflight.compile_count.load(Ordering::Relaxed),
            preflight.validation_count.load(Ordering::Relaxed),
        ))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "tool dispatch must keep active-turn accounting atomic"
    )]
    pub(crate) async fn dispatch_any_with_terminal_outcome(
        &self,
        mut invocation: ToolInvocation,
        dispatch_state: Arc<ToolDispatchState>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        let tool_name = invocation.tool_name.clone();
        let mut parsed_function_arguments =
            ParsedFunctionArguments::from_payload(&invocation.payload);
        let mut rewritten_source_dependencies = None;
        let tool_name_flat = flat_tool_name(&tool_name);
        let call_id_owned = invocation.call_id.clone();
        let otel = invocation.step_context.turn.session_telemetry.clone();
        let base_tool_result_tags = [
            (
                "sandbox",
                permission_profile_sandbox_tag(
                    &invocation.step_context.turn.permission_profile,
                    invocation.step_context.turn.windows_sandbox_level,
                    invocation.step_context.turn.network.is_some(),
                ),
            ),
            (
                "sandbox_policy",
                permission_profile_policy_tag(
                    &invocation.step_context.turn.permission_profile,
                    invocation.step_context.turn.cwd().as_path(),
                ),
            ),
        ];

        {
            let mut active = invocation.session.active_turn.lock().await;
            if let Some(active_turn) = active.as_mut() {
                let mut turn_state = active_turn.turn_state.lock().await;
                turn_state.tool_calls = turn_state.tool_calls.saturating_add(1);
            }
        }
        let dispatch_trace = ToolDispatchTrace::start(&invocation);
        let (tool, tool_spec, code_mode_argument_preflight) = match self.tools.get(&tool_name) {
            Some(registered) => (
                Arc::clone(registered.runtime()),
                registered.spec().clone(),
                Arc::clone(&registered.code_mode_argument_preflight),
            ),
            None => {
                let message = unsupported_tool_call_message(&invocation.payload, &tool_name);
                let log_payload = invocation.payload.log_payload();
                otel.tool_result_with_tags(
                    tool_name_flat.as_ref(),
                    &call_id_owned,
                    log_payload.as_ref(),
                    Duration::ZERO,
                    /*success*/ false,
                    &message,
                    &base_tool_result_tags,
                    /*extra_trace_fields*/ &[],
                );
                let err = FunctionCallError::RespondToModel(message);
                dispatch_trace.record_failed(&err).await;
                return Err(err);
            }
        };

        let telemetry_tags = tool.telemetry_tags(&invocation).await;
        let mut tool_result_tags =
            Vec::with_capacity(base_tool_result_tags.len() + telemetry_tags.len());
        let mut extra_trace_fields = Vec::new();
        tool_result_tags.extend_from_slice(&base_tool_result_tags);
        for (key, value) in &telemetry_tags {
            if matches!(*key, "mcp_server" | "mcp_server_origin") {
                extra_trace_fields.push((*key, value.as_str()));
            } else {
                tool_result_tags.push((*key, value.as_str()));
            }
        }
        if !tool.matches_kind(&invocation.payload) {
            let message = format!("tool {tool_name} invoked with incompatible payload");
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::Fatal(message);
            dispatch_trace.record_failed(&err).await;
            return Err(err);
        }

        if matches!(invocation.source, ToolCallSource::CodeMode { .. })
            && let Err(message) = code_mode_argument_preflight.validate(
                &tool_name,
                &tool_spec,
                &invocation.payload,
                parsed_function_arguments.as_ref(),
            )
        {
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::RespondToModel(message);
            dispatch_trace.record_failed(&err).await;
            return Err(err);
        }

        let phase_started = Instant::now();
        notify_tool_start(&invocation).await;
        record_lifecycle_phase(&invocation, "notify_start", phase_started);

        let mut hook_rewrote_input = false;
        let pre_tool_use_payload =
            with_parsed_function_arguments(parsed_function_arguments.clone(), async {
                tool.pre_tool_use_payload(&invocation)
            })
            .await;
        if let Some(pre_tool_use_payload) = pre_tool_use_payload {
            let phase_started = Instant::now();
            let pre_tool_use_result = run_pre_tool_use_hooks(
                &invocation.session,
                &invocation.step_context.turn,
                invocation.call_id.clone(),
                &pre_tool_use_payload.tool_name,
                &pre_tool_use_payload.tool_input,
            )
            .await;
            record_lifecycle_phase(&invocation, "pre_hooks", phase_started);
            match pre_tool_use_result {
                PreToolUseHookResult::Blocked(message) => {
                    let err = FunctionCallError::RespondToModel(message);
                    dispatch_trace.record_failed(&err).await;
                    notify_tool_finish_if_unclaimed(
                        &invocation,
                        dispatch_state.as_ref(),
                        ToolCallOutcome::Blocked,
                    )
                    .await;
                    return Err(err);
                }
                PreToolUseHookResult::Continue {
                    updated_input: Some(updated_input),
                } => {
                    match with_parsed_function_arguments(parsed_function_arguments.clone(), async {
                        tool.with_updated_hook_input(invocation.clone(), updated_input)
                    })
                    .await
                    {
                        Ok(updated_invocation) => {
                            invocation = updated_invocation;
                            parsed_function_arguments =
                                ParsedFunctionArguments::from_payload(&invocation.payload);
                            hook_rewrote_input = true;
                            let rewritten_tool_name = flat_tool_name(&invocation.tool_name);
                            rewritten_source_dependencies = crate::tool_history::tool_observes_workspace(
                                rewritten_tool_name.as_ref(),
                            )
                            .then(|| {
                                crate::tool_history::source_dependencies_for_tool_call_with_parsed_arguments(
                                    rewritten_tool_name.as_ref(),
                                    &invocation.payload,
                                    parsed_function_arguments
                                        .as_ref()
                                        .and_then(|parsed| parsed.value().ok()),
                                    invocation.step_context.turn.config.cwd.as_path(),
                                )
                            });
                        }
                        Err(err) => {
                            dispatch_trace.record_failed(&err).await;
                            notify_tool_finish_if_unclaimed(
                                &invocation,
                                dispatch_state.as_ref(),
                                ToolCallOutcome::Failed {
                                    handler_executed: false,
                                },
                            )
                            .await;
                            return Err(err);
                        }
                    }
                }
                PreToolUseHookResult::Continue {
                    updated_input: None,
                } => {}
            }
        }

        // PreToolUse hooks may replace the arguments after the initial code-mode
        // admission check. Validate the final payload that will reach the
        // handler so a hook cannot turn a schema-valid call into an invalid one.
        if hook_rewrote_input
            && matches!(invocation.source, ToolCallSource::CodeMode { .. })
            && let Err(message) = code_mode_argument_preflight.validate(
                &tool_name,
                &tool_spec,
                &invocation.payload,
                parsed_function_arguments.as_ref(),
            )
        {
            let log_payload = invocation.payload.log_payload();
            otel.tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                Duration::ZERO,
                /*success*/ false,
                &message,
                &tool_result_tags,
                &extra_trace_fields,
            );
            let err = FunctionCallError::RespondToModel(message);
            dispatch_trace.record_failed(&err).await;
            notify_tool_finish_if_unclaimed(
                &invocation,
                dispatch_state.as_ref(),
                ToolCallOutcome::Failed {
                    handler_executed: false,
                },
            )
            .await;
            return Err(err);
        }

        let invocation_for_tool = invocation.clone();
        let log_payload = invocation.payload.log_payload();

        let phase_started = Instant::now();
        let mut result = otel
            .log_tool_result_with_tags(
                tool_name_flat.as_ref(),
                &call_id_owned,
                log_payload.as_ref(),
                &tool_result_tags,
                &extra_trace_fields,
                || {
                    let tool = tool.clone();
                    let parsed_function_arguments = parsed_function_arguments.clone();
                    async move {
                        with_parsed_function_arguments(
                            parsed_function_arguments,
                            handle_any_tool(tool.as_ref(), invocation_for_tool),
                        )
                        .await
                    }
                },
                AnyToolResult::success_for_logging,
                |result| result.result.log_preview(),
            )
            .await;
        record_lifecycle_phase(&invocation, "handler", phase_started);
        let success = match &result {
            Ok(result) => result.success_for_logging(),
            Err(_) => false,
        };
        emit_metric_for_tool_read(&invocation, success);
        let post_tool_use_plan = success
            .then(|| tool.post_tool_use_hook_name(&invocation))
            .flatten()
            .and_then(|tool_name| {
                let plan = invocation
                    .session
                    .hooks()
                    .plan_post_tool_use(tool_name.name(), tool_name.matcher_aliases());
                (!plan.is_empty()).then_some((tool_name, plan))
            });
        let post_tool_use_payload = if let Some((tool_name, plan)) = post_tool_use_plan {
            let payload =
                with_parsed_function_arguments(parsed_function_arguments.clone(), async {
                    result.as_mut().ok().and_then(|result| {
                        CoreToolRuntime::post_tool_use_payload(
                            tool.as_ref(),
                            &invocation,
                            result.result.as_ref(),
                        )
                    })
                })
                .await;
            payload.map(|payload| {
                debug_assert_eq!(payload.tool_name, tool_name);
                (plan, payload)
            })
        } else {
            None
        };
        let post_tool_use_outcome =
            if let Some((plan, post_tool_use_payload)) = post_tool_use_payload {
                let phase_started = Instant::now();
                let outcome = run_post_tool_use_hooks(
                    &invocation.session,
                    &invocation.step_context.turn,
                    plan,
                    post_tool_use_payload.tool_use_id,
                    post_tool_use_payload.tool_name.name().to_string(),
                    post_tool_use_payload.tool_name.matcher_aliases().to_vec(),
                    post_tool_use_payload.tool_input,
                    post_tool_use_payload.tool_response,
                )
                .await;
                record_lifecycle_phase(&invocation, "post_hooks", phase_started);
                Some(outcome)
            } else {
                None
            };
        if let (Ok(result), Some(outcome)) = (&mut result, post_tool_use_outcome) {
            let phase_started = Instant::now();
            let additional_contexts = apply_post_tool_use_outcome(result, outcome);
            if matches!(&invocation.source, ToolCallSource::Direct) {
                let contexts = prepare_additional_context_items(
                    &invocation.session,
                    &invocation.step_context.turn,
                    additional_contexts,
                )
                .await;
                invocation
                    .step_context
                    .turn
                    .queue_post_tool_contexts(&invocation.call_id, contexts)
                    .await;
            } else {
                record_additional_contexts(
                    &invocation.session,
                    &invocation.step_context.turn,
                    additional_contexts,
                )
                .await;
            }
            record_lifecycle_phase(&invocation, "additional_context", phase_started);
        }

        // A cooperative runtime can finish teardown after cancellation has
        // already claimed the terminal outcome. Do not continue into
        // projection, persistence, or finish notification for that result.
        if dispatch_state.is_aborted() {
            let err = FunctionCallError::RespondToModel(
                "tool cancelled after runtime cleanup".to_string(),
            );
            dispatch_trace.record_failed(&err).await;
            return Err(err);
        }

        // A PostToolUse block rejects the result, not the already-completed tool execution.
        let lifecycle_outcome = match &result {
            Ok(result) => match result.outcome_for_logging() {
                ToolOutputOutcome::Skipped => ToolCallOutcome::Skipped,
                ToolOutputOutcome::Success | ToolOutputOutcome::Yielded => {
                    ToolCallOutcome::Completed { success: true }
                }
                ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => {
                    ToolCallOutcome::Completed { success: false }
                }
            },
            Err(_) => ToolCallOutcome::Failed {
                handler_executed: true,
            },
        };
        match result {
            Ok(mut result) => {
                let force_inline_carrier = result
                    .deterministic_continuation_owner_key()
                    .is_some_and(|owner_key| {
                        !invocation
                            .session
                            .services
                            .code_mode_service
                            .owner_drained_continuation_snapshot(&owner_key)
                            .is_empty()
                    });
                let canonical_artifact_required = result.result.requires_canonical_artifact();
                let provider_visible = projection_is_provider_visible(&invocation.source);
                let projection_admission_required = projection_admission_required(
                    &invocation.source,
                    &invocation.tool_name,
                    force_inline_carrier,
                );
                let admission_tracking_enabled = admission_tracking_enabled(
                    &invocation.source,
                    invocation
                        .step_context
                        .turn
                        .config
                        .completed_tool_history_projection,
                );
                let phase_started = Instant::now();
                let projection_input = prepare_model_projection(
                    &invocation,
                    &result,
                    parsed_function_arguments.as_ref(),
                    rewritten_source_dependencies.as_ref(),
                    force_inline_carrier,
                    projection_admission_required,
                );
                let admission_tracking_required =
                    projection_admission_required && projection_input.is_some();
                let model_projection = match projection_input {
                    Some(input) => project_model_output(input).await,
                    None => None,
                };
                record_lifecycle_phase(&invocation, "projection", phase_started);
                if (canonical_artifact_required || admission_tracking_required)
                    && model_projection.is_none()
                {
                    tracing::error!(
                        tool_name = %flat_tool_name(&invocation.tool_name),
                        call_id = %invocation.call_id,
                        "tool execution completed but its canonical result could not be preserved"
                    );
                    let model_visible = FunctionToolOutput::from_text(
                        "Tool execution completed, but its full result could not be preserved for model delivery."
                            .to_string(),
                        Some(false),
                    );
                    result.result = Box::new(UnavailableModelProjectionOutput {
                        original: result.result,
                        model_visible,
                    });
                }
                if let Some(projection) = &model_projection {
                    invocation
                        .step_context
                        .turn
                        .turn_timing_state
                        .record_tool_output_projection_facts(
                            projection.canonical_bytes,
                            projection.canonical_tokens,
                            projection.model_bytes,
                            projection.projected_tokens,
                            projection.artifact_created,
                            projection.artifact_reused,
                            projection.projection_truncated,
                            projection.omitted_sections,
                            provider_visible,
                        );
                    if admission_tracking_enabled && let Some(candidate) = &projection.candidate {
                        let phase_started = Instant::now();
                        invocation
                            .session
                            .register_tool_history_candidate(
                                invocation.step_context.turn.config.codex_home.as_path(),
                                candidate.clone(),
                            )
                            .await;
                        record_history_persistence(phase_started.elapsed());
                    }
                }
                result.install_model_projection(model_projection, rewritten_source_dependencies);
                let phase_started = Instant::now();
                notify_tool_finish(&invocation, lifecycle_outcome).await;
                record_lifecycle_phase(&invocation, "notify_finish", phase_started);
                dispatch_trace
                    .record_completed(
                        &invocation,
                        &result.call_id,
                        &result.payload,
                        result.result.as_ref(),
                    )
                    .await;
                if dispatch_state.try_complete() {
                    Ok(result)
                } else {
                    Err(FunctionCallError::RespondToModel(
                        "tool cancelled after runtime cleanup".to_string(),
                    ))
                }
            }
            Err(err) => {
                let phase_started = Instant::now();
                notify_tool_finish(&invocation, lifecycle_outcome).await;
                record_lifecycle_phase(&invocation, "notify_finish", phase_started);
                dispatch_trace.record_failed(&err).await;
                if dispatch_state.try_complete() {
                    Err(err)
                } else {
                    Err(FunctionCallError::RespondToModel(
                        "tool cancelled after runtime cleanup".to_string(),
                    ))
                }
            }
        }
    }
}

fn projection_is_provider_visible(source: &ToolCallSource) -> bool {
    matches!(source, ToolCallSource::Direct)
}

fn admission_tracking_enabled(source: &ToolCallSource, configured: bool) -> bool {
    configured && projection_is_provider_visible(source)
}

fn projection_admission_required(
    source: &ToolCallSource,
    tool_name: &ToolName,
    force_inline_carrier: bool,
) -> bool {
    projection_is_provider_visible(source)
        && (crate::tools::code_mode::is_exec_tool_name(tool_name)
            || !generic_projection_is_exempt(tool_name, force_inline_carrier))
}

async fn notify_tool_finish_if_unclaimed(
    invocation: &ToolInvocation,
    dispatch_state: &ToolDispatchState,
    outcome: ToolCallOutcome,
) -> bool {
    if dispatch_state.is_aborted() {
        return false;
    }

    notify_tool_finish(invocation, outcome).await;
    dispatch_state.try_complete()
}

async fn handle_any_tool(
    tool: &dyn CoreToolRuntime,
    invocation: ToolInvocation,
) -> Result<AnyToolResult, FunctionCallError> {
    let _tool_execution_timing_guard =
        matches!(tool.tool_execution_timing(), ToolExecutionTiming::Handler).then(|| {
            invocation
                .step_context
                .turn
                .turn_timing_state
                .begin_tool_execution()
        });
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_tool_handler_entry(invocation.tool_name.name.as_str());
    mark_tool_handler_entry();
    let output = tool.handle(invocation.clone()).await;
    mark_tool_handler_exit();
    let output = output?;
    if output.contains_external_context()
        && invocation
            .step_context
            .turn
            .config
            .memories
            .disable_on_external_context
        && invocation.step_context.turn.claim_memory_pollution_signal()
    {
        state_integration::mark_thread_memory_mode_polluted(
            invocation.session.services.state_db.as_deref(),
            invocation.session.thread_id,
            "tool_output",
        )
        .await;
    }
    Ok(AnyToolResult {
        call_id: invocation.call_id,
        payload: invocation.payload,
        result: output,
        model_projection: None,
        source_dependencies: None,
        code_mode_feedback: Vec::new(),
    })
}

struct ModelProjectionInput {
    spillable_text: String,
    outcome: ToolOutputOutcome,
    essential_inline: Value,
    origin_call_id: String,
    selection_facts: ProjectionSelectionFacts,
    applied_token_limit: usize,
    projected_text: String,
    preserved_content: Vec<Value>,
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
    projection_eligible: bool,
    projection_truncated: bool,
    predetermined_ranges: Vec<ToolOutputProjectionRange>,
    predetermined_json_pointers: Vec<ToolOutputProjectionJsonPointer>,
    original_response: ResponseInputItem,
    materialization: ProjectionMaterialization,
}

tokio::task_local! {
    static PRECOMPUTED_PROJECTION_SOURCE_DEPENDENCIES:
        std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>;
}

pub(crate) async fn with_precomputed_projection_source_dependencies<F>(
    source_dependencies: Option<
        std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    >,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    match source_dependencies {
        Some(source_dependencies) => {
            PRECOMPUTED_PROJECTION_SOURCE_DEPENDENCIES
                .scope(source_dependencies, future)
                .await
        }
        None => future.await,
    }
}

fn precomputed_projection_source_dependencies()
-> Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>> {
    PRECOMPUTED_PROJECTION_SOURCE_DEPENDENCIES
        .try_with(Clone::clone)
        .ok()
}

fn resolve_projection_source_dependencies<F>(
    turn_timing_state: &TurnTimingState,
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
        turn_timing_state.record_projection_source_dependencies_fallback();
        return source_dependencies;
    }
    match precomputed {
        Some(source_dependencies) => {
            turn_timing_state.record_projection_source_dependencies_reuse();
            source_dependencies
        }
        None => {
            turn_timing_state.record_projection_source_dependencies_fallback();
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

fn prepare_model_projection(
    invocation: &ToolInvocation,
    result: &AnyToolResult,
    parsed_function_arguments: Option<&ParsedFunctionArguments>,
    source_dependencies_override: Option<
        &std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    >,
    force_inline_carrier: bool,
    track_for_admission: bool,
) -> Option<ModelProjectionInput> {
    // Exact artifact reads are already bounded and must never recursively spill.
    // Code mode also performs its own coherent outer projection after merging
    // native nested results. Keep the first `functions.exec` result byte-for-byte
    // provider-visible, but admit it to completed-tool history so later
    // generations can replace consumed output with an exact-artifact receipt.
    let admit_code_mode_output = crate::tools::code_mode::is_exec_tool_name(&invocation.tool_name)
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
    if admit_code_mode_output && metadata.outcome == ToolOutputOutcome::Yielded {
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
    let limits = resolve_projected_output_limits(
        requested_limit,
        outcome,
        diagnostic_class,
        DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
    );
    let generic_projection = formatted_truncate_text_with_output_limit(&spillable_text, limits);
    let non_text_tokens = non_text_projection_token_cost(&preserved_content);
    // Truncation already proves the canonical text exceeds the applied budget.
    // Avoid a second full-buffer token scan on the large-output path; the
    // canonical producer remains the accounting source for the artifact.
    let model_output_tokens = if generic_projection.was_truncated {
        limits.applied_limit.saturating_add(1)
    } else {
        approx_token_count(&spillable_text)
    }
    .saturating_add(non_text_tokens);
    let projection_truncated =
        generic_projection.was_truncated || model_output_tokens > limits.applied_limit;
    let has_predetermined_selectors = !metadata.predetermined_ranges.is_empty()
        || !metadata.predetermined_json_pointers.is_empty();
    let needs_canonical_artifact = result.result.requires_canonical_artifact()
        || projection_truncated
        || has_predetermined_selectors;
    if !needs_canonical_artifact && !force_inline_carrier && !track_for_admission {
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
    let (projected_text, selection_facts) =
        if materialization == ProjectionMaterialization::InlineCarrier {
            (
                spillable_text.clone(),
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
                generic_projection.text,
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
            select_typed_projection_fragments(&metadata.fragments, applied_token_limit)
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
    let source_dependencies = resolve_projection_source_dependencies(
        invocation.step_context.turn.turn_timing_state.as_ref(),
        source_dependencies_override.cloned(),
        precomputed_projection_source_dependencies(),
        || {
            crate::tool_history::source_dependencies_for_tool_call_with_parsed_arguments(
                flat_tool_name(&invocation.tool_name).as_ref(),
                &invocation.payload,
                parsed_function_arguments.and_then(|parsed| parsed.value().ok()),
                invocation.step_context.turn.config.cwd.as_path(),
            )
        },
    );
    let original_output_sha256 = crate::tool_history::sha256(original_output_text.as_bytes());
    let original_output_tokens = if generic_projection.was_truncated {
        canonical
            .approximate_tokens
            .saturating_add(non_text_tokens as u64)
    } else {
        model_output_tokens as u64
    };
    let invocation_sha256 =
        canonical_tool_invocation_sha256(&invocation.payload, parsed_function_arguments);
    Some(ModelProjectionInput {
        spillable_text,
        outcome: metadata.outcome,
        essential_inline: metadata.essential_inline,
        origin_call_id: result.call_id.clone(),
        selection_facts,
        applied_token_limit,
        projected_text,
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
        projection_eligible: true,
        projection_truncated,
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
    let normalized =
        projected_response_item(original, text, /*preserve_non_text_content*/ true);
    history_output_text(&normalized).map(|normalized_text| (normalized, normalized_text))
}

fn history_output_text(response: &ResponseInputItem) -> Option<String> {
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

fn consolidated_history_output_text(response: &ResponseInputItem) -> String {
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

fn canonical_tool_invocation_sha256(
    payload: &ToolPayload,
    parsed_function_arguments: Option<&ParsedFunctionArguments>,
) -> Option<String> {
    let value = match payload {
        ToolPayload::Function { arguments } => serde_json::json!({
            "kind": "function",
            "arguments": match parsed_function_arguments {
                Some(parsed) => parsed
                    .value()
                    .map(canonicalize_json)
                    .unwrap_or_else(|_| Value::String(arguments.clone())),
                None => canonical_json_argument(arguments),
            },
        }),
        ToolPayload::ToolSearch { arguments } => serde_json::json!({
            "kind": "tool_search",
            "arguments": canonicalize_json(&serde_json::to_value(arguments).ok()?),
        }),
        ToolPayload::Custom { input } => serde_json::json!({
            "kind": "custom",
            "input": canonical_json_argument(input),
        }),
    };
    let bytes = serde_json::to_vec(&canonicalize_json(&value)).ok()?;
    Some(crate::tool_history::sha256(&bytes))
}

fn canonical_json_argument(value: &str) -> Value {
    serde_json::from_str(value)
        .map(|value| canonicalize_json(&value))
        .unwrap_or_else(|_| Value::String(value.to_string()))
}

fn generic_projection_is_exempt(tool_name: &ToolName, force_inline_carrier: bool) -> bool {
    tool_name.name == "read_tool_output"
        // MCP handlers already own provider-safe truncation and typed content
        // shaping. Re-projecting their results can replace image content with
        // a text artifact and can supersede distinct parallel call/output
        // pairs when their arguments and results happen to be identical.
        || tool_name
            .namespace
            .as_deref()
            .is_some_and(|namespace| namespace.starts_with("mcp__"))
        || (crate::tools::code_mode::is_exec_tool_name(tool_name) && !force_inline_carrier)
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
        .filter(|fragment| seen.insert((*fragment).clone()))
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
    let mut sections = Vec::new();
    let mut selected_fragments = 0;
    let mut selected_ids = Vec::new();
    let mut partial_ids = Vec::new();

    for kind in PROJECTION_FRAGMENT_KIND_ORDER {
        let section_fragments = unique
            .iter()
            .filter(|fragment| fragment.kind == kind)
            .collect::<Vec<_>>();
        if section_fragments.is_empty() {
            continue;
        }
        let mut remaining_budget = section_budget;
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
            let bounded = bounded_fragment_text(&fragment.text, remaining_budget);
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

    let projected = truncate_text_to_token_ceiling(&sections.join("\n\n"), token_limit);
    let selected_id_set = selected_ids.iter().collect::<HashSet<_>>();
    let omitted_inline_ids = unique
        .iter()
        .filter_map(|fragment| fragment.id.as_ref())
        .filter(|id| !selected_id_set.contains(id))
        .cloned()
        .collect();
    (
        projected,
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

async fn project_model_output(input: ModelProjectionInput) -> Option<ModelToolProjection> {
    let ModelProjectionInput {
        spillable_text: _spillable_text,
        outcome,
        essential_inline,
        origin_call_id,
        selection_facts,
        mut applied_token_limit,
        projected_text,
        mut preserved_content,
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
        projection_eligible,
        projection_truncated,
        predetermined_ranges,
        predetermined_json_pointers,
        original_response,
        materialization,
    } = input;
    let successful = !matches!(
        &outcome,
        ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut
    );
    if !projection_eligible {
        return None;
    }
    let total_applied_token_limit = applied_token_limit;
    // Make the canonical artifact durable before spending time on the inline
    // projection. If projection later fails or is cancelled, recovery still
    // has the complete canonical output to work from.
    let persisted_artifact = if materialization == ProjectionMaterialization::InlineCarrier {
        None
    } else {
        let existing_artifact_id = essential_inline
            .get("raw_output_artifact_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let artifact_created = existing_artifact_id.is_none();
        let artifact = if let Some(artifact_id) = existing_artifact_id {
            attach_canonical_output_artifact(&codex_home, &thread_id, &artifact_id, &canonical)
                .await
        } else {
            create_canonical_output_artifact(&codex_home, &thread_id, &canonical).await
        };
        let artifact_id = artifact.artifact_id();
        if let Some(artifact_id) = artifact_id
            && artifact.complete
            && artifact.retained_bytes == canonical.exact_bytes
            && artifact.unavailable_ranges.is_empty()
            && crate::tools::command_output_artifact::protect_active_tool_history_artifact(
                &codex_home,
                &thread_id,
                &artifact_id,
                canonical.exact_bytes,
                &canonical.sha256,
            )
            .await
            .is_ok()
        {
            Some((artifact, artifact_id, artifact_created))
        } else {
            None
        }
    };
    let non_text_tokens = non_text_projection_token_cost(&preserved_content);
    let non_text_bytes = non_text_projection_byte_cost(&preserved_content);
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
    let admission_only_fallback = || {
        let bounded = BoundedModelProjection::Fallback {
            value: serde_json::from_str(&original_output_text)
                .unwrap_or_else(|_| Value::String(original_output_text.clone())),
            rendered: original_output_text.clone(),
        };
        ModelToolProjection {
            original_response: original_response.clone(),
            bounded,
            passthrough_response: true,
            preserve_non_text_content: true,
            candidate: None,
            projected_tokens: approx_token_count(&original_output_text)
                .saturating_add(non_text_tokens) as u64,
            canonical_bytes: canonical.exact_bytes,
            canonical_tokens: canonical.approximate_tokens,
            model_bytes: original_output_text.len().saturating_add(non_text_bytes) as u64,
            artifact_created: false,
            artifact_reused: false,
            projection_truncated: false,
            omitted_sections: 0,
            deterministic_continuation_receipt: None,
            deterministic_continuation_content: Vec::new(),
            source_dependencies: source_dependencies.clone(),
            applied_token_limit: total_applied_token_limit,
        }
    };
    let Some((artifact, artifact_id, artifact_created)) = persisted_artifact else {
        return (materialization == ProjectionMaterialization::AdmissionOnly)
            .then(admission_only_fallback);
    };
    let supersession_identity = invocation_sha256
        .map(|invocation_sha256| format!("{tool_name}:{invocation_sha256}:{}", canonical.sha256));
    if materialization == ProjectionMaterialization::AdmissionOnly {
        let bounded = BoundedModelProjection::Fallback {
            value: serde_json::from_str(&original_output_text)
                .unwrap_or_else(|_| Value::String(original_output_text.clone())),
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
                preserved_non_text_tokens: non_text_tokens as u64,
                bounded_model_output: original_output_text.clone(),
                complete: canonical.complete,
                projection_eligible,
                proof_identity: None,
                supersession_identity,
                consumed_by_generation: None,
                derived: Default::default(),
            }),
            projected_tokens: approx_token_count(&original_output_text)
                .saturating_add(non_text_tokens) as u64,
            canonical_bytes: canonical.exact_bytes,
            canonical_tokens: canonical.approximate_tokens,
            model_bytes: original_output_text.len().saturating_add(non_text_bytes) as u64,
            artifact_created,
            artifact_reused: !artifact_created,
            projection_truncated: false,
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
    let omitted_section_count = omitted_sections.len() as u64;
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
    let (predetermined_ranges, predetermined_json_pointers) =
        validated_omitted_predetermined_selectors(
            &predetermined_ranges,
            &predetermined_json_pointers,
            &canonical.sections,
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
            &canonical.sections,
            &canonical.json_pointers,
            applied_token_limit,
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
    let mut bounded =
        serialize_projection_with_limit(envelope, &projected_text, applied_token_limit)?;
    if bounded.envelope().is_none_or(|envelope| {
        !drained_content_survived(envelope, preserved_content_start, &drained_content)
    }) {
        deterministic_continuation_receipt = None;
        bounded =
            serialize_projection_with_limit(base_envelope, &projected_text, applied_token_limit)?;
    }
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
            preserved_non_text_tokens: retained_non_text_tokens as u64,
            bounded_model_output: rendered,
            complete: canonical.complete,
            projection_eligible,
            proof_identity: None,
            supersession_identity,
            consumed_by_generation: None,
            derived: Default::default(),
        }),
        projected_tokens,
        canonical_bytes: canonical.exact_bytes,
        canonical_tokens: canonical.approximate_tokens,
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
    let Ok((result, _reused)) = read_tool_output_selectors_with_ceiling_and_reuse(
        codex_home,
        thread_id,
        artifact_id,
        selectors.clone(),
        token_ceiling,
    )
    .await
    else {
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

#[cfg(test)]
async fn drain_predetermined_artifact_ranges(
    codex_home: &std::path::Path,
    thread_id: &str,
    artifact_id: &str,
    state_revision: &str,
    ranges: Vec<ToolOutputProjectionRange>,
    sections: &[ToolProjectionSection],
) -> (
    Vec<Value>,
    Option<TurnTimingDeterministicContinuationReceipt>,
) {
    drain_predetermined_artifact_selectors(
        codex_home,
        thread_id,
        artifact_id,
        state_revision,
        ranges,
        Vec::new(),
        sections,
        &BTreeMap::new(),
        codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS,
    )
    .await
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
        let covering = actual
            .iter()
            .filter(|actual| actual.start >= expected.start && actual.end <= expected.end)
            .collect::<Vec<_>>();
        covering
            .first()
            .is_some_and(|range| range.start == expected.start)
            && covering
                .last()
                .is_some_and(|range| range.end == expected.end)
            && covering.windows(2).all(|pair| pair[0].end == pair[1].start)
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
    mut envelope: ToolProjectionV1,
    output: &str,
    token_limit: usize,
) -> Option<BoundedModelProjection> {
    // Spillable text remains subject to the requested limit. Structured control
    // state is not spillable, so the smallest typed carrier containing
    // `essential` defines a safe floor when that carrier alone exceeds the
    // request.
    let effective_limit = token_limit.max(1);
    let mut output_limit = effective_limit;
    loop {
        envelope.result["selected_text"] =
            Value::String(truncate_text_to_token_ceiling(output, output_limit));
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
            .min(output_limit - 1);
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
    let mut header = serde_json::Map::new();
    header.insert(
        "outcome".to_string(),
        Value::String(envelope.outcome.clone()),
    );
    header.insert(
        "canonical_complete".to_string(),
        Value::Bool(envelope.canonical_complete),
    );
    if let Some(artifact_id) = envelope.artifact_id.as_ref() {
        header.insert(
            "artifact_id".to_string(),
            Value::String(artifact_id.clone()),
        );
    }
    if !envelope.omitted_sections.is_empty() {
        header.insert(
            "omitted_sections".to_string(),
            serde_json::to_value(&envelope.omitted_sections).ok()?,
        );
    }
    for field in ["essential", "preserved_content", "artifact"] {
        if let Some(value) = envelope.result.get(field)
            && !value.is_null()
            && !value.as_array().is_some_and(Vec::is_empty)
            && !value.as_object().is_some_and(serde_json::Map::is_empty)
        {
            header.insert(field.to_string(), value.clone());
        }
    }
    if let Some(selection) = envelope.result["selection"].as_object() {
        let mut compact_selection = serde_json::Map::new();
        for field in ["selected_ids", "omitted_inline_ids", "partial_ids"] {
            if let Some(value) = selection.get(field)
                && !value.as_array().is_some_and(Vec::is_empty)
            {
                compact_selection.insert(field.to_string(), value.clone());
            }
        }
        if !compact_selection.is_empty() {
            header.insert("selection".to_string(), Value::Object(compact_selection));
        }
    }
    if !selected_text.is_empty() {
        header.insert("selected_text_follows".to_string(), Value::Bool(true));
    }

    let mut rendered = serde_json::to_string(&header).ok()?;
    if !selected_text.is_empty() {
        rendered.push('\n');
        rendered.push_str(selected_text);
    }
    envelope.model_bytes = rendered.len() as u64;
    envelope.model_approximate_tokens = approx_token_count(&rendered) as u64;
    Some(rendered)
}

fn preserved_non_text_content(response: &ResponseInputItem) -> Vec<Value> {
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
    serde_json::to_string(content)
        .map(|serialized| approx_token_count(&serialized))
        .unwrap_or(usize::MAX)
}

fn non_text_projection_byte_cost(content: &[Value]) -> usize {
    serde_json::to_vec(content)
        .map(|serialized| serialized.len())
        .unwrap_or(usize::MAX)
}

fn function_hook_tool_name(invocation: &ToolInvocation) -> HookToolName {
    if invocation.tool_name.name == "spawn_agent"
        && matches!(
            invocation.tool_name.namespace.as_deref(),
            None | Some(MULTI_AGENT_V1_NAMESPACE)
        )
    {
        return HookToolName::spawn_agent();
    }

    HookToolName::new(flat_tool_name(&invocation.tool_name).into_owned())
}

fn compile_code_mode_argument_validator(
    tool_name: &ToolName,
    spec: &ToolSpec,
) -> Result<Option<jsonschema::Validator>, String> {
    let parameters = match spec {
        ToolSpec::Function(tool) => Some(&tool.parameters),
        ToolSpec::Namespace(namespace) => namespace.tools.iter().find_map(|nested| match nested {
            codex_tools::ResponsesApiNamespaceTool::Function(tool)
                if tool.name == tool_name.name =>
            {
                Some(&tool.parameters)
            }
            codex_tools::ResponsesApiNamespaceTool::Function(_) => None,
        }),
        ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } | ToolSpec::Freeform(_) => None,
    };
    let Some(parameters) = parameters else {
        return Ok(None);
    };
    let schema = serde_json::to_value(parameters)
        .map_err(|err| format!("tool `{tool_name}` argument schema is invalid: {err}"))?;
    let validator = jsonschema::validator_for(&schema).map_err(|err| {
        format!("tool `{tool_name}` argument schema could not be compiled: {err}")
    })?;
    Ok(Some(validator))
}

#[derive(Default)]
struct CodeModeArgumentPreflight {
    validator: OnceLock<Result<Option<jsonschema::Validator>, String>>,
    #[cfg(test)]
    compile_count: AtomicUsize,
    #[cfg(test)]
    validation_count: AtomicUsize,
}

impl CodeModeArgumentPreflight {
    fn validate(
        &self,
        tool_name: &ToolName,
        spec: &ToolSpec,
        payload: &ToolPayload,
        parsed_function_arguments: Option<&ParsedFunctionArguments>,
    ) -> Result<(), String> {
        let ToolPayload::Function { arguments } = payload else {
            return Ok(());
        };
        if tool_name.namespace.is_none() && tool_name.name == "exec_command" {
            crate::tools::handlers::validate_exec_command_arguments(arguments).map_err(
                |message| format!("tool `{tool_name}` argument preflight failed: {message}"),
            )?;
        }
        let value = parsed_function_arguments
            .and_then(|parsed| parsed.value().ok())
            .cloned()
            .ok_or_else(|| {
                let message = parsed_function_arguments
                    .and_then(|parsed| parsed.value().err())
                    .unwrap_or("unknown JSON parse error");
                format!("tool `{tool_name}` arguments are not valid JSON: {message}")
            })?;
        if !value.is_object() {
            return Err(format!(
                "tool `{tool_name}` expects a JSON object for arguments"
            ));
        }

        let validator = self.validator.get_or_init(|| {
            #[cfg(test)]
            self.compile_count.fetch_add(1, Ordering::Relaxed);
            compile_code_mode_argument_validator(tool_name, spec)
        });
        let validator = validator.as_ref().map_err(Clone::clone)?;
        let Some(validator) = validator else {
            return Ok(());
        };
        #[cfg(test)]
        self.validation_count.fetch_add(1, Ordering::Relaxed);
        let mut errors = validator.iter_errors(&value);
        let messages = errors
            .by_ref()
            .take(4)
            .map(|error| {
                let pointer = error.instance_path().as_str();
                let path = if pointer.is_empty() {
                    "$".to_string()
                } else {
                    format!("${pointer}")
                };
                format!("{path}: {error}")
            })
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }
        let suffix = errors
            .next()
            .is_some()
            .then_some("; additional errors omitted");
        Err(format!(
            "tool `{tool_name}` argument preflight failed: {}{}",
            messages.join("; "),
            suffix.unwrap_or_default()
        ))
    }
}

fn function_hook_tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }

    match parsed_function_argument_value(arguments) {
        Some(Ok(value)) => value,
        Some(Err(_)) => Value::String(arguments.to_string()),
        None => {
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
        }
    }
}

fn unsupported_tool_call_message(payload: &ToolPayload, tool_name: &ToolName) -> String {
    match payload {
        ToolPayload::Custom { .. } => format!("unsupported custom tool call: {tool_name}"),
        _ => format!("unsupported call: {tool_name}"),
    }
}
#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
