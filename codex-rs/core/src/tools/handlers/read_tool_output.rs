use crate::FunctionCallError;
use crate::tools::command_output_artifact::RECOVERY_AGGREGATE_TOKEN_CEILING;
use crate::tools::command_output_artifact::ReadToolOutputError;
use crate::tools::command_output_artifact::ReadToolOutputResult;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::ToolOutputSelectorResult;
use crate::tools::command_output_artifact::ToolOutputSelectorStatus;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_ceiling_and_reuse;
use crate::tools::command_output_artifact::read_tool_output_selectors_with_reuse;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::context::semantic_evidence_for_command_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_BYTES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_LEGACY_RANGES;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_MAX_SELECTORS;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_TOOL_NAME;
use crate::tools::handlers::read_tool_output_spec::create_read_tool_output_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_tools::CanonicalToolResult;
use codex_tools::JsonToolOutput;
use codex_tools::ToolName;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputProjectionMetadata;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::path::Path;
use tokio_util::sync::CancellationToken;

const DEFAULT_LINE_COUNT: usize = 200;
const MAX_AGGREGATE_LINES: usize = 2_000;
// A nested result is serialized into a code-mode cell and then into the outer
// exec result. Reserve enough space for that outer envelope so a fitting exact
// recovery cannot be recursively truncated into another artifact.
const CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS: usize = 1_000;
const CODE_MODE_RECOVERY_TOKEN_CEILING: usize =
    codex_utils_output_truncation::DEFAULT_SUCCESS_OUTPUT_TOKENS
        .saturating_sub(CODE_MODE_RECOVERY_WRAPPER_RESERVE_TOKENS);

#[derive(Debug)]
struct DrainedRecoveryTransaction {
    output: ReadToolOutputResult,
    reused: bool,
    drained_continuation_pages: u32,
    continuation_stop: Option<RecoveryContinuationStopV1>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ContinuationStopReason {
    Budget,
    Cancelled,
    IdentityDrift,
    IncompleteOwnerResult,
    InvalidSelector,
    SelectorNotFound,
    PageReadError,
    RepeatedSelector,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RecoveryContinuationStopV1 {
    version: u8,
    reason: ContinuationStopReason,
    selector: Option<ToolOutputSelector>,
    resumable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ContinuationStep {
    Complete,
    Follow {
        result_index: usize,
        selector: ToolOutputSelector,
    },
    Stop(ContinuationStopReason),
}

struct RecoveryContinuationState {
    output: ReadToolOutputResult,
    reused: bool,
    followed_selectors: Vec<ToolOutputSelector>,
    drained_continuation_pages: u32,
    token_ceiling: usize,
    continuation_stop: Option<RecoveryContinuationStopV1>,
}

impl RecoveryContinuationState {
    fn new(output: ReadToolOutputResult, reused: bool, token_ceiling: usize) -> Self {
        Self {
            output,
            reused,
            followed_selectors: Vec::new(),
            drained_continuation_pages: 0,
            token_ceiling,
            continuation_stop: None,
        }
    }

    fn record_stop(
        &mut self,
        reason: ContinuationStopReason,
        selector: Option<ToolOutputSelector>,
    ) {
        self.continuation_stop = Some(RecoveryContinuationStopV1 {
            version: 1,
            reason,
            selector,
            resumable: matches!(
                reason,
                ContinuationStopReason::Budget | ContinuationStopReason::Cancelled
            ),
            message: Some(match reason {
                ContinuationStopReason::Budget => "Recovery reached its output budget. Continue with the unconsumed selector in a new call.",
                ContinuationStopReason::Cancelled => "Recovery was cancelled. Already recovered pages are retained; the unconsumed selector can be retried.",
                ContinuationStopReason::IdentityDrift => "The artifact identity or continuation changed. Re-identify the artifact and start a new recovery; do not combine pages from different identities.",
                ContinuationStopReason::IncompleteOwnerResult => "The artifact has unavailable ranges or returned an incomplete continuation result. Inspect the retained evidence and obtain the missing source before claiming complete recovery.",
                ContinuationStopReason::InvalidSelector => "The selector is invalid. Correct it using the selector result message and the read_tool_output schema.",
                ContinuationStopReason::SelectorNotFound => "The selector did not find the requested content. Inspect the artifact structure or revise the selector.",
                ContinuationStopReason::RepeatedSelector => "The continuation repeated an already consumed selector. Stop this traversal and choose a different selector.",
                ContinuationStopReason::PageReadError => "The continuation page could not be read.",
            }.to_string()),
        });
    }

    fn record_page_read_error(
        &mut self,
        error: &ReadToolOutputError,
        selector: ToolOutputSelector,
    ) {
        self.continuation_stop = Some(RecoveryContinuationStopV1 {
            version: 1,
            reason: ContinuationStopReason::PageReadError,
            selector: Some(selector),
            resumable: matches!(error, ReadToolOutputError::StillWriting),
            message: Some(error.for_model()),
        });
    }

    fn first_pending_selector(&self) -> Option<ToolOutputSelector> {
        self.output.results.iter().find_map(|result| {
            if selector_stop_reason(result.status).is_some() {
                Some(result.selector.clone())
            } else {
                result.continuation.clone()
            }
        })
    }

    fn next_step(&self) -> ContinuationStep {
        if !self.output.unavailable_ranges.is_empty() {
            return ContinuationStep::Stop(ContinuationStopReason::IncompleteOwnerResult);
        }
        for (result_index, result) in self.output.results.iter().enumerate() {
            if let Some(reason) = selector_stop_reason(result.status) {
                return ContinuationStep::Stop(reason);
            }
            let Some(selector) = result.continuation.as_ref() else {
                continue;
            };
            if self.followed_selectors.contains(selector) {
                return ContinuationStep::Stop(ContinuationStopReason::RepeatedSelector);
            }
            return ContinuationStep::Follow {
                result_index,
                selector: selector.clone(),
            };
        }
        ContinuationStep::Complete
    }

    fn accept_page(
        &mut self,
        result_index: usize,
        selector: &ToolOutputSelector,
        page: ReadToolOutputResult,
        page_reused: bool,
    ) -> Result<(), ContinuationStopReason> {
        self.reused &= page_reused;
        let Some(predecessor) = self.output.results.get(result_index) else {
            return Err(ContinuationStopReason::IncompleteOwnerResult);
        };
        if predecessor.continuation.as_ref() != Some(selector)
            || page.artifact_id != self.output.artifact_id
            || page.canonical_sha256 != self.output.canonical_sha256
        {
            return Err(ContinuationStopReason::IdentityDrift);
        }
        if !page.unavailable_ranges.is_empty()
            || page.results.len() != 1
            || &page.results[0].selector != selector
        {
            return Err(ContinuationStopReason::IncompleteOwnerResult);
        }
        if let Some(reason) = selector_stop_reason(page.results[0].status) {
            return Err(reason);
        }

        let previous_length = self.output.results.len();
        let previous_complete = self.output.complete;
        let predecessor = &mut self.output.results[result_index];
        let next_continuation = next_owner_continuation(predecessor, selector);
        let previous_continuation =
            std::mem::replace(&mut predecessor.continuation, next_continuation);
        let predecessor_was_complete = predecessor.complete;
        if predecessor.status == ToolOutputSelectorStatus::Ok {
            predecessor.complete = predecessor.continuation.is_none();
        }
        // Keep already-drained pages in traversal order. Inserting every page
        // immediately after the owner reverses multi-page continuations.
        self.output.results.extend(page.results);
        self.output.complete = self.output.unavailable_ranges.is_empty()
            && self.output.results.iter().all(|result| {
                result.status == ToolOutputSelectorStatus::Ok
                    && result.complete
                    && result.continuation.is_none()
            });
        if !recovery_result_fits_token_ceiling(&self.output, self.token_ceiling) {
            // Roll back only this page rather than copying every accumulated
            // page for each budget check.
            self.output.results.truncate(previous_length);
            self.output.complete = previous_complete;
            let predecessor = &mut self.output.results[result_index];
            predecessor.continuation = previous_continuation;
            predecessor.complete = predecessor_was_complete;
            return Err(ContinuationStopReason::Budget);
        }

        self.followed_selectors.push(selector.clone());
        self.drained_continuation_pages = self.drained_continuation_pages.saturating_add(1);
        Ok(())
    }

    fn finish(self) -> DrainedRecoveryTransaction {
        DrainedRecoveryTransaction {
            output: self.output,
            reused: self.reused,
            drained_continuation_pages: self.drained_continuation_pages,
            continuation_stop: self.continuation_stop,
        }
    }
}

fn selector_stop_reason(status: ToolOutputSelectorStatus) -> Option<ContinuationStopReason> {
    match status {
        ToolOutputSelectorStatus::Invalid => Some(ContinuationStopReason::InvalidSelector),
        ToolOutputSelectorStatus::NotFound => Some(ContinuationStopReason::SelectorNotFound),
        ToolOutputSelectorStatus::Ok
        | ToolOutputSelectorStatus::SelectorTooLarge
        | ToolOutputSelectorStatus::AggregateOmitted => None,
    }
}

fn next_owner_continuation(
    predecessor: &ToolOutputSelectorResult,
    consumed: &ToolOutputSelector,
) -> Option<ToolOutputSelector> {
    if let (
        Some(plan),
        ToolOutputSelector::Bytes {
            end: consumed_end, ..
        },
    ) = (predecessor.subdivision_plan.as_ref(), consumed)
        && *consumed_end < plan.range.end
    {
        return Some(ToolOutputSelector::Bytes {
            start: *consumed_end,
            end: consumed_end
                .saturating_add(plan.chunk_bytes.max(1))
                .min(plan.range.end),
        });
    }

    predecessor
        .child_selectors
        .iter()
        .position(|child| child == consumed)
        .and_then(|index| predecessor.child_selectors.get(index.saturating_add(1)))
        .cloned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolOutputArgs {
    artifact_id: String,
    #[serde(default)]
    selectors: Option<Vec<ToolOutputSelector>>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    ranges: Option<Vec<ReadToolOutputRangeArgs>>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolOutputRangeArgs {
    start_line: usize,
    end_line: usize,
}

pub struct ReadToolOutputHandler;

struct ReadToolOutputToolOutput {
    inner: JsonToolOutput,
    exact_recovery: Option<TurnTimingDeterministicContinuationReceipt>,
    semantic_evidence: Vec<String>,
}

impl ToolOutput for ReadToolOutputToolOutput {
    fn log_preview(&self) -> String {
        self.inner.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.inner.success_for_logging()
    }

    fn sampling_request_signal(&self) -> Option<Value> {
        Some(serde_json::json!({
            "kind": "semantic_evidence",
            "semantic_evidence": self.semantic_evidence,
        }))
    }

    fn deterministic_continuation_receipts(
        &self,
    ) -> Vec<TurnTimingDeterministicContinuationReceipt> {
        self.exact_recovery
            .as_ref()
            .map(|receipt| vec![receipt.clone()])
            .unwrap_or_default()
    }

    fn deterministic_continuation_content(&self) -> Vec<Value> {
        self.exact_recovery
            .as_ref()
            .map(|_| vec![self.inner.value().clone()])
            .unwrap_or_default()
    }

    fn projection_metadata(&self) -> Option<ToolOutputProjectionMetadata> {
        self.inner.projection_metadata()
    }

    fn canonical_result(&self, payload: &ToolPayload) -> Option<CanonicalToolResult> {
        self.inner.canonical_result(payload)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.inner.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.inner.code_mode_result(payload)
    }
}

impl ToolExecutor<ToolInvocation> for ReadToolOutputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_read_tool_output_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_read_tool_output(invocation))
    }
}

impl CoreToolRuntime for ReadToolOutputHandler {}

async fn handle_read_tool_output(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_tool_output received unsupported payload".to_string(),
        ));
    };
    let args = parse_read_tool_output_args(arguments)?;
    // Keep validating the legacy knob for compatibility, but never use it to
    // clip a selected value. The selector engine owns its exact response fit.
    let _legacy_max_bytes = resolved_max_bytes(args.max_bytes)?;
    let selectors = resolved_selectors(&args)?;
    let code_mode_recovery = matches!(&invocation.source, ToolCallSource::CodeMode { .. });
    let mut action_bounds_digest = Sha256::new();
    serde_json::to_writer(&mut action_bounds_digest, &selectors).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery selectors: {err}"))
    })?;
    let action_bounds_hash = format!("{:x}", action_bounds_digest.finalize());
    let transaction = execute_recovery_transaction_with_continuations(
        invocation.step_context.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        &args.artifact_id,
        selectors,
        code_mode_recovery,
        &invocation.cancellation_token,
    )
    .await
    .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;
    let DrainedRecoveryTransaction {
        output,
        reused,
        drained_continuation_pages,
        continuation_stop,
    } = transaction;
    if !reused {
        invocation
            .step_context
            .turn
            .turn_timing_state
            .record_tool_output_artifact_reread();
    }
    // Typed overflow reports an incomplete selection without losing retained
    // evidence. Recovery bypasses recursive spilling, so no retruncation occurs.
    invocation
        .step_context
        .turn
        .turn_timing_state
        .record_tool_output_recovery(/*retruncation_count*/ 0);

    let exact_recovery_receipt = exact_code_mode_recovery_receipt(
        code_mode_recovery,
        &output,
        action_bounds_hash,
        drained_continuation_pages,
    );
    let semantic_evidence = read_tool_output_semantic_evidence(&output, continuation_stop.as_ref());
    let mut output = serde_json::to_value(output).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to serialize recovery result: {err}"))
    })?;
    if let (Some(stop), Some(object)) = (continuation_stop, output.as_object_mut()) {
        object.insert(
            "continuation_stop".to_string(),
            serde_json::to_value(stop).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to serialize recovery continuation stop: {err}"
                ))
            })?,
        );
    }
    Ok(boxed_tool_output(ReadToolOutputToolOutput {
        inner: JsonToolOutput::new(output),
        exact_recovery: exact_recovery_receipt,
        semantic_evidence,
    }))
}

fn parse_read_tool_output_args(arguments: &str) -> Result<ReadToolOutputArgs, FunctionCallError> {
    parse_arguments(arguments).map_err(|err| match err {
        FunctionCallError::RespondToModel(message) => {
            let detail = message
                .strip_prefix("failed to parse function arguments: ")
                .unwrap_or(&message);
            FunctionCallError::RespondToModel(format!(
                "failed to parse read_tool_output arguments: {detail}. Consult the advertised read_tool_output schema."
            ))
        }
        err => err,
    })
}

fn read_tool_output_semantic_evidence(
    output: &ReadToolOutputResult,
    continuation_stop: Option<&RecoveryContinuationStopV1>,
) -> Vec<String> {
    let mut recovered_fragments = output
        .results
        .iter()
        .filter(|result| result.status == ToolOutputSelectorStatus::Ok)
        .filter_map(|result| result.text.as_deref().map(|text| (result, text)))
        .peekable();
    if recovered_fragments.peek().is_some() {
        let mut evidence = Vec::new();
        let mut seen_facts = HashSet::new();
        for (result, recovered_fragment) in recovered_fragments {
            let fragment_facts =
                semantic_evidence_for_command_output(recovered_fragment.as_bytes());
            let provenance = serde_json::to_vec(&serde_json::json!({
                "canonical_sha256": output.canonical_sha256,
                "selector": result.selector,
                "canonical_range": result.canonical_range,
                "facts": fragment_facts,
            }))
            .unwrap_or_default();
            for fact in fragment_facts {
                if !seen_facts.contains(&fact) {
                    seen_facts.insert(fact.clone());
                    evidence.push(fact);
                }
            }
            evidence.push(format!(
                "artifact-recovery-fragment-v1:{}",
                crate::tool_history::sha256(&provenance)
            ));
        }
        let supplemental_results = output
            .results
            .iter()
            .filter(|result| {
                result.status != ToolOutputSelectorStatus::Ok
                    || !result.complete
                    || result.text.is_none()
            })
            .collect::<Vec<_>>();
        if !output.complete
            || !output.unavailable_ranges.is_empty()
            || !supplemental_results.is_empty()
        {
            let supplemental = serde_json::to_vec(&serde_json::json!({
                "complete": output.complete,
                "unavailable_ranges": output.unavailable_ranges,
                "results": supplemental_results,
                "continuation_stop": continuation_stop,
            }))
            .unwrap_or_default();
            evidence.push(format!(
                "artifact-recovery-status-v1:{}",
                crate::tool_history::sha256(&supplemental)
            ));
        }
        return evidence;
    }
    let recovered_complete_artifact = output.complete
        && output.unavailable_ranges.is_empty()
        && output.results.iter().any(|result| {
            result.status == ToolOutputSelectorStatus::Ok
                && result.complete
                && result
                    .canonical_range
                    .is_some_and(|range| range.start == 0 && range.end == output.canonical_bytes)
        });
    if recovered_complete_artifact {
        return vec![format!("canonical-output-v1:{}", output.canonical_sha256)];
    }
    let projection = serde_json::to_vec(&serde_json::json!({
        "canonical_sha256": output.canonical_sha256,
        "complete": output.complete,
        "unavailable_ranges": output.unavailable_ranges,
        "results": output.results,
        "continuation_stop": continuation_stop,
    }))
    .unwrap_or_default();
    vec![format!(
        "artifact-projection-v1:{}",
        crate::tool_history::sha256(&projection)
    )]
}

pub(crate) async fn execute_recovery_transaction(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    code_mode_recovery: bool,
) -> Result<(ReadToolOutputResult, bool), ReadToolOutputError> {
    if code_mode_recovery {
        read_tool_output_selectors_with_ceiling_and_reuse(
            codex_home,
            thread_id,
            artifact_id,
            selectors,
            CODE_MODE_RECOVERY_TOKEN_CEILING,
        )
        .await
    } else {
        read_tool_output_selectors_with_reuse(codex_home, thread_id, artifact_id, selectors).await
    }
}

async fn execute_recovery_transaction_with_continuations(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    code_mode_recovery: bool,
    cancellation_token: &CancellationToken,
) -> Result<DrainedRecoveryTransaction, ReadToolOutputError> {
    let (output, reused) = execute_recovery_transaction(
        codex_home,
        thread_id,
        artifact_id,
        selectors,
        code_mode_recovery,
    )
    .await?;
    let token_ceiling = if code_mode_recovery {
        CODE_MODE_RECOVERY_TOKEN_CEILING
    } else {
        RECOVERY_AGGREGATE_TOKEN_CEILING
    };
    let mut state = RecoveryContinuationState::new(output, reused, token_ceiling);
    loop {
        let (result_index, selector) = match state.next_step() {
            ContinuationStep::Complete => break,
            ContinuationStep::Stop(reason) => {
                let selector = state.first_pending_selector();
                state.record_stop(reason, selector);
                break;
            }
            ContinuationStep::Follow {
                result_index,
                selector,
            } => (result_index, selector),
        };
        if cancellation_token.is_cancelled() {
            state.record_stop(ContinuationStopReason::Cancelled, Some(selector));
            break;
        }
        let page = execute_recovery_transaction(
            codex_home,
            thread_id,
            artifact_id,
            vec![selector.clone()],
            code_mode_recovery,
        )
        .await;
        if cancellation_token.is_cancelled() {
            state.record_stop(ContinuationStopReason::Cancelled, Some(selector));
            break;
        }
        let (page, page_reused) = match page {
            Ok(page) => page,
            Err(error) => {
                state.record_page_read_error(&error, selector);
                break;
            }
        };
        if let Err(reason) = state.accept_page(result_index, &selector, page, page_reused) {
            state.record_stop(reason, Some(selector));
            break;
        }
    }
    Ok(state.finish())
}

fn recovery_result_fits_token_ceiling(output: &ReadToolOutputResult, token_ceiling: usize) -> bool {
    serde_json::to_string(output)
        .is_ok_and(|rendered| codex_utils_string::approx_token_count(&rendered) <= token_ceiling)
}

fn exact_code_mode_recovery_receipt(
    code_mode_recovery: bool,
    output: &crate::tools::command_output_artifact::ReadToolOutputResult,
    action_bounds_hash: String,
    suppressed_continuation_count: u32,
) -> Option<TurnTimingDeterministicContinuationReceipt> {
    (code_mode_recovery
        && suppressed_continuation_count > 0
        && output.unavailable_ranges.is_empty()
        && !output.results.is_empty()
        && output.results.iter().all(|result| {
            matches!(
                result.status,
                ToolOutputSelectorStatus::Ok
                    | ToolOutputSelectorStatus::SelectorTooLarge
                    | ToolOutputSelectorStatus::AggregateOmitted
            )
        }))
    .then(|| TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::ArtifactRange,
        wire_identity: String::new(),
        resource_identity_hash: crate::tool_history::sha256(output.artifact_id.as_bytes()),
        state_revision: output.canonical_sha256.clone(),
        host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
        action_bounds_hash,
        suppressed_continuation_count,
    })
}

fn resolved_selectors(
    args: &ReadToolOutputArgs,
) -> Result<Vec<ToolOutputSelector>, FunctionCallError> {
    if let Some(selectors) = &args.selectors {
        if selectors.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "selectors must contain at least one selector".to_string(),
            ));
        }
        if selectors.len() > READ_TOOL_OUTPUT_MAX_SELECTORS {
            return Err(FunctionCallError::RespondToModel(format!(
                "selectors may contain at most {READ_TOOL_OUTPUT_MAX_SELECTORS} entries"
            )));
        }
        if args.start_line.is_some() || args.end_line.is_some() || args.ranges.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "selectors cannot be combined with legacy line arguments".to_string(),
            ));
        }
        return Ok(selectors.clone());
    }
    if let Some(ranges) = &args.ranges {
        if args.start_line.is_some() || args.end_line.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "ranges is mutually exclusive with start_line/end_line".to_string(),
            ));
        }
        if ranges.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "ranges must contain at least one range".to_string(),
            ));
        }
        if ranges.len() > READ_TOOL_OUTPUT_MAX_LEGACY_RANGES {
            return Err(FunctionCallError::RespondToModel(format!(
                "ranges may contain at most {READ_TOOL_OUTPUT_MAX_LEGACY_RANGES} entries"
            )));
        }
        let ranges = ranges
            .iter()
            .map(|range| {
                if range.start_line == 0 || range.end_line < range.start_line {
                    Err(FunctionCallError::RespondToModel(
                        "each range requires 1-based start_line <= end_line".to_string(),
                    ))
                } else {
                    Ok((range.start_line, range.end_line))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let normalized = crate::tools::command_output_artifact::normalize_line_ranges(ranges);
        let aggregate_lines = normalized.iter().try_fold(0_usize, |total, (start, end)| {
            total.checked_add(end - start + 1)
        });
        if aggregate_lines.is_none_or(|lines| lines > MAX_AGGREGATE_LINES) {
            return Err(FunctionCallError::RespondToModel(format!(
                "ranges may request at most {MAX_AGGREGATE_LINES} aggregate lines"
            )));
        }
        return Ok(normalized
            .into_iter()
            .map(|(start, end)| ToolOutputSelector::Lines { start, end })
            .collect());
    }
    let (start, end) = resolved_line_range(args)?;
    Ok(vec![ToolOutputSelector::Lines { start, end }])
}

fn resolved_line_range(args: &ReadToolOutputArgs) -> Result<(usize, usize), FunctionCallError> {
    let start_line = args.start_line.unwrap_or(1);
    let end_line = match args.end_line {
        Some(end_line) => end_line,
        None => start_line
            .checked_add(DEFAULT_LINE_COUNT - 1)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("start_line is too large".to_string())
            })?,
    };
    if start_line == 0 || end_line < start_line {
        return Err(FunctionCallError::RespondToModel(
            "line ranges require 1-based start_line <= end_line".to_string(),
        ));
    }
    Ok((start_line, end_line))
}

fn resolved_max_bytes(max_bytes: Option<usize>) -> Result<usize, FunctionCallError> {
    match max_bytes {
        Some(max_bytes) if max_bytes == 0 || max_bytes > READ_TOOL_OUTPUT_MAX_BYTES => {
            Err(FunctionCallError::RespondToModel(format!(
                "max_bytes must be between 1 and {READ_TOOL_OUTPUT_MAX_BYTES}"
            )))
        }
        Some(max_bytes) => Ok(max_bytes),
        None => Ok(READ_TOOL_OUTPUT_MAX_BYTES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::command_output_artifact::ByteSubdivisionPlan;

    #[tokio::test]
    async fn recovery_handler_output_schema_covers_exact_search_and_rejected_selectors() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let session = std::sync::Arc::new(session);
        let turn = std::sync::Arc::new(turn);
        let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
            &turn.config.codex_home,
            &session.thread_id.to_string(),
            &CanonicalToolResult::json(serde_json::json!({"present": "value"})),
        )
        .await;
        let artifact_id = artifact
            .artifact_id()
            .expect("retained artifact")
            .to_string();
        let spec = ReadToolOutputHandler.spec();
        let ToolSpec::Function(spec) = spec else {
            panic!("recovery uses a function spec");
        };
        let validator =
            jsonschema::validator_for(spec.output_schema.as_ref().expect("output schema"))
                .expect("valid output schema");
        for (selector, expected_status, expected_reason) in [
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "invalid"}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/present~2"}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/present~"}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/missing"}),
                "not_found",
                Some("selector_not_found"),
            ),
            (
                serde_json::json!({"kind": "lines", "start": 0, "end": 0}),
                "invalid",
                Some("invalid_selector"),
            ),
            (
                serde_json::json!({"kind": "json_pointer", "pointer": "/present"}),
                "ok",
                None,
            ),
            (
                serde_json::json!({"kind": "search", "query": "present"}),
                "ok",
                None,
            ),
        ] {
            let payload = ToolPayload::Function {
                arguments: serde_json::json!({
                    "artifact_id": artifact_id,
                    "selectors": [selector],
                })
                .to_string(),
            };
            let result = ReadToolOutputHandler
                .handle(ToolInvocation {
                    session: std::sync::Arc::clone(&session),
                    step_context: crate::session::step_context::StepContext::for_test(
                        std::sync::Arc::clone(&turn),
                    ),
                    cancellation_token: Default::default(),
                    tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
                        crate::turn_diff_tracker::TurnDiffTracker::new(),
                    )),
                    call_id: "selector-recovery".to_string(),
                    tool_name: ToolName::plain("read_tool_output"),
                    source: ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .expect("handler output")
                .code_mode_result(&payload);
            assert!(
                validator.is_valid(&result),
                "schema rejected handler result: {result}"
            );
            assert_eq!(
                result["results"][0]["status"], expected_status,
                "{selector}"
            );
            if let Some(reason) = expected_reason {
                assert_eq!(result["continuation_stop"]["reason"], reason);
                assert_eq!(result["continuation_stop"]["selector"], selector);
                assert_eq!(result["continuation_stop"]["resumable"], false);
                assert!(
                    !result["continuation_stop"]["message"]
                        .as_str()
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(result["complete"], false);
                let mut malformed = result.clone();
                malformed["continuation_stop"]["reason"] = serde_json::json!("unknown");
                assert!(!validator.is_valid(&malformed));
            } else {
                assert_eq!(result["complete"], true);
                assert!(result.get("continuation_stop").is_none());
                if selector["kind"] == "search" {
                    assert_eq!(result["results"][0]["value"]["total_matches"], 1);
                    assert_eq!(
                        result["results"][0]["value"]["hydrated_ranges"][0]["text"],
                        r#"{"present":"value"}"#
                    );
                    let mut malformed = result.clone();
                    malformed["results"][0]["value"]["hydrated_ranges"] =
                        serde_json::json!("missing ranges");
                    assert!(!validator.is_valid(&malformed));
                } else {
                    assert_eq!(result["results"][0]["value"], "value");
                }
            }
            let mut malformed = result;
            malformed["complete"] = serde_json::json!("true");
            assert!(!validator.is_valid(&malformed));
        }
    }

    use codex_tools::CanonicalByteRange;

    fn selector_result(status: ToolOutputSelectorStatus) -> ToolOutputSelectorResult {
        ToolOutputSelectorResult {
            selector: ToolOutputSelector::Lines { start: 1, end: 1 },
            status,
            complete: status == ToolOutputSelectorStatus::Ok,
            exact_bytes: None,
            canonical_range: None,
            text: None,
            value: None,
            data_base64: None,
            subdivision_plan: None,
            child_selectors: Vec::new(),
            continuation: None,
            message: None,
        }
    }

    fn search_selector(start_byte: u64) -> ToolOutputSelector {
        ToolOutputSelector::Search {
            query: "needle".to_string(),
            start_byte,
            max_results: 1,
            context_lines: 0,
        }
    }

    fn continuation_result(
        selector: ToolOutputSelector,
        continuation: Option<ToolOutputSelector>,
        text: &str,
    ) -> ToolOutputSelectorResult {
        ToolOutputSelectorResult {
            selector,
            status: ToolOutputSelectorStatus::Ok,
            complete: continuation.is_none(),
            exact_bytes: Some(text.len() as u64),
            canonical_range: None,
            text: Some(text.to_string()),
            value: None,
            data_base64: None,
            subdivision_plan: None,
            child_selectors: Vec::new(),
            continuation,
            message: None,
        }
    }

    fn recovery_output(results: Vec<ToolOutputSelectorResult>) -> ReadToolOutputResult {
        ReadToolOutputResult {
            artifact_id: "01900000-0000-7000-8000-000000000000".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 100,
            retained_bytes: 100,
            complete: results
                .iter()
                .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete),
            unavailable_ranges: Vec::new(),
            results,
        }
    }

    #[test]
    fn terminal_recovery_results_complete_after_the_validation_pass() {
        let mut ok = continuation_result(search_selector(0), None, "ok");
        ok.status = ToolOutputSelectorStatus::Ok;
        let mut oversized = continuation_result(search_selector(10), None, "oversized");
        oversized.status = ToolOutputSelectorStatus::SelectorTooLarge;
        let mut omitted = continuation_result(search_selector(20), None, "omitted");
        omitted.status = ToolOutputSelectorStatus::AggregateOmitted;
        let state = RecoveryContinuationState::new(
            recovery_output(vec![ok, oversized, omitted]),
            false,
            usize::MAX,
        );

        assert_eq!(state.next_step(), ContinuationStep::Complete);
    }

    #[test]
    fn exact_continuation_pages_are_drained_in_selector_order() {
        let first_selector = search_selector(0);
        let second_selector = search_selector(10);
        let initial = recovery_output(vec![continuation_result(
            first_selector.clone(),
            Some(second_selector.clone()),
            "first page",
        )]);
        let page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            None,
            "second page",
        )]);
        let retained_text = initial.results[0].text.as_ref().unwrap().as_ptr();
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: second_selector.clone(),
            }
        );
        assert_eq!(state.accept_page(0, &second_selector, page, true), Ok(()));
        assert_eq!(
            state.output.results[0].text.as_ref().unwrap().as_ptr(),
            retained_text,
            "accepting a page must retain previously recovered text without copying it",
        );
        assert_eq!(state.next_step(), ContinuationStep::Complete);

        let transaction = state.finish();
        assert_eq!(transaction.drained_continuation_pages, 1);
        assert!(!transaction.reused);
        assert!(transaction.output.complete);
        assert_eq!(
            transaction
                .output
                .results
                .iter()
                .map(|result| result.selector.clone())
                .collect::<Vec<_>>(),
            vec![first_selector, second_selector]
        );
        assert!(
            transaction
                .output
                .results
                .iter()
                .all(|result| result.continuation.is_none())
        );
    }

    #[test]
    fn continuation_budget_stop_preserves_first_unconsumed_selector() {
        let first_selector = search_selector(0);
        let second_selector = search_selector(10);
        let initial = recovery_output(vec![continuation_result(
            first_selector,
            Some(second_selector.clone()),
            "first page",
        )]);
        let initial_tokens = codex_utils_string::approx_token_count(
            &serde_json::to_string(&initial).expect("serialize initial page"),
        );
        let oversized_page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            None,
            &"x".repeat(10_000),
        )]);
        let mut state = RecoveryContinuationState::new(initial.clone(), true, initial_tokens + 1);

        assert_eq!(
            state.accept_page(0, &second_selector, oversized_page, false),
            Err(ContinuationStopReason::Budget)
        );
        let transaction = state.finish();
        assert_eq!(transaction.output, initial);
        assert_eq!(transaction.drained_continuation_pages, 0);
        assert!(!transaction.reused);
        assert_eq!(
            transaction.output.results[0].continuation,
            Some(second_selector)
        );
    }

    #[test]
    fn continuation_identity_drift_stops_without_mutating_the_aggregate() {
        let second_selector = search_selector(10);
        let initial = recovery_output(vec![continuation_result(
            search_selector(0),
            Some(second_selector.clone()),
            "first page",
        )]);
        let mut drifted_page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            None,
            "second page",
        )]);
        drifted_page.canonical_sha256 = "different-revision".to_string();
        let mut state = RecoveryContinuationState::new(initial.clone(), false, usize::MAX);

        assert_eq!(
            state.accept_page(0, &second_selector, drifted_page, false),
            Err(ContinuationStopReason::IdentityDrift)
        );
        assert_eq!(state.finish().output, initial);
    }

    #[test]
    fn aggregate_omission_may_retry_its_owner_advertised_selector_once() {
        let selector = ToolOutputSelector::Lines { start: 1, end: 10 };
        let mut omitted = selector_result(ToolOutputSelectorStatus::AggregateOmitted);
        omitted.selector = selector.clone();
        omitted.continuation = Some(selector.clone());
        let initial = recovery_output(vec![omitted]);
        let page = recovery_output(vec![continuation_result(
            selector.clone(),
            None,
            "exact retry",
        )]);
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: selector.clone(),
            }
        );
        assert_eq!(state.accept_page(0, &selector, page, false), Ok(()));
        assert_eq!(state.next_step(), ContinuationStep::Complete);
    }

    #[test]
    fn repeated_owner_continuation_is_never_followed_twice() {
        let second_selector = search_selector(10);
        let initial = recovery_output(vec![continuation_result(
            search_selector(0),
            Some(second_selector.clone()),
            "first page",
        )]);
        let repeated_page = recovery_output(vec![continuation_result(
            second_selector.clone(),
            Some(second_selector.clone()),
            "second page",
        )]);
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.accept_page(0, &second_selector, repeated_page, false),
            Ok(())
        );
        assert_eq!(
            state.next_step(),
            ContinuationStep::Stop(ContinuationStopReason::RepeatedSelector)
        );
        let transaction = state.finish();
        assert_eq!(transaction.drained_continuation_pages, 1);
        assert_eq!(
            transaction.output.results[1].continuation,
            Some(second_selector)
        );
    }

    #[test]
    fn selector_overflow_drains_the_owner_subdivision_plan_and_preserves_its_contract() {
        let parent_selector = ToolOutputSelector::Lines { start: 1, end: 100 };
        let child_selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
        let second_child_selector = ToolOutputSelector::Bytes { start: 10, end: 20 };
        let mut overflow = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
        overflow.selector = parent_selector;
        overflow.canonical_range = Some(CanonicalByteRange { start: 0, end: 20 });
        overflow.subdivision_plan = Some(ByteSubdivisionPlan {
            range: CanonicalByteRange { start: 0, end: 20 },
            chunk_bytes: 10,
            chunk_count: 2,
            selector_kind: "bytes".to_string(),
        });
        overflow.child_selectors = vec![child_selector.clone()];
        overflow.continuation = Some(child_selector.clone());
        let initial = recovery_output(vec![overflow]);
        let first_page = recovery_output(vec![continuation_result(
            child_selector.clone(),
            None,
            "exact child",
        )]);
        let second_page = recovery_output(vec![continuation_result(
            second_child_selector.clone(),
            None,
            "second exact child",
        )]);
        let mut state = RecoveryContinuationState::new(initial, false, usize::MAX);

        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: child_selector.clone(),
            }
        );
        assert_eq!(
            state.accept_page(0, &child_selector, first_page, false),
            Ok(())
        );
        assert_eq!(
            state.next_step(),
            ContinuationStep::Follow {
                result_index: 0,
                selector: second_child_selector.clone(),
            }
        );
        assert_eq!(
            state.accept_page(0, &second_child_selector, second_page, false),
            Ok(())
        );
        assert_eq!(state.next_step(), ContinuationStep::Complete);
        let transaction = state.finish();
        assert!(!transaction.output.complete);
        assert_eq!(
            transaction.output.results[0].status,
            ToolOutputSelectorStatus::SelectorTooLarge
        );
        assert_eq!(
            transaction.output.results[0].child_selectors,
            vec![child_selector]
        );
        assert!(transaction.output.results[0].continuation.is_none());
        assert_eq!(transaction.drained_continuation_pages, 2);
        assert_eq!(
            transaction.output.results[2].selector,
            second_child_selector
        );
    }

    #[test]
    fn typed_overflow_preserves_artifact_completeness() {
        let output = crate::tools::command_output_artifact::ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "digest".to_string(),
            canonical_bytes: 1,
            retained_bytes: 1,
            complete: false,
            unavailable_ranges: Vec::new(),
            results: vec![
                selector_result(ToolOutputSelectorStatus::Ok),
                selector_result(ToolOutputSelectorStatus::SelectorTooLarge),
                selector_result(ToolOutputSelectorStatus::AggregateOmitted),
                selector_result(ToolOutputSelectorStatus::NotFound),
            ],
        };

        let delivered = serde_json::to_value(output).expect("recovery result");
        assert_eq!(delivered["retained_artifact_complete"], true);
        assert_eq!(delivered["delivered_selection_complete"], false);
    }

    #[test]
    fn exact_code_mode_recovery_is_carried_by_owner_receipt() {
        let output = crate::tools::command_output_artifact::ReadToolOutputResult {
            artifact_id: "01900000-0000-7000-8000-000000000000".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 5,
            retained_bytes: 5,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![selector_result(ToolOutputSelectorStatus::Ok)],
        };

        let receipt =
            exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string(), 2)
                .expect("exact nested recovery receipt");

        assert_eq!(receipt.state_revision, "canonical-revision");
        assert_eq!(receipt.action_bounds_hash, "selector-bounds");
        assert_eq!(receipt.suppressed_continuation_count, 2);
        assert!(
            exact_code_mode_recovery_receipt(false, &output, "selector-bounds".to_string(), 2,)
                .is_none()
        );
        assert!(
            exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string(), 0,)
                .is_none()
        );
        let value = serde_json::to_value(output).expect("recovery output");
        let mut tool_output = ReadToolOutputToolOutput {
            inner: JsonToolOutput::new(value.clone()),
            exact_recovery: Some(receipt.clone()),
            semantic_evidence: Vec::new(),
        };
        assert_eq!(
            tool_output.deterministic_continuation_receipts(),
            vec![receipt]
        );
        assert_eq!(
            tool_output.deterministic_continuation_content(),
            vec![value]
        );
        tool_output.exact_recovery = None;
        assert!(tool_output.deterministic_continuation_receipts().is_empty());
        assert!(tool_output.deterministic_continuation_content().is_empty());
    }

    #[test]
    fn complete_artifact_recovery_reuses_the_producers_canonical_identity() {
        let mut result = selector_result(ToolOutputSelectorStatus::Ok);
        result.canonical_range = Some(codex_tools::CanonicalByteRange::new(0, 5));
        let output = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 5,
            retained_bytes: 5,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![result],
        };

        assert_eq!(
            read_tool_output_semantic_evidence(&output, None),
            vec!["canonical-output-v1:canonical-revision"]
        );
    }

    #[test]
    fn recovered_text_reuses_the_command_fact_identity() {
        let mut result = selector_result(ToolOutputSelectorStatus::Ok);
        result.text = Some("src/lib.rs:10:let stable = compute();".to_string());
        let output = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 40,
            retained_bytes: 40,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![result],
        };

        let expected = semantic_evidence_for_command_output(
            b"diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+let stable = compute();",
        );
        let evidence = read_tool_output_semantic_evidence(&output, None);

        assert!(expected.iter().all(|fact| evidence.contains(fact)));
        assert_eq!(
            evidence
                .iter()
                .filter(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
                .count(),
            1
        );
        // Repeated selections keep their provenance but must not duplicate or
        // reorder the semantic facts already emitted for the first fragment.
        let provenance = evidence
            .iter()
            .find(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
            .expect("fragment provenance")
            .clone();
        let mut repeated = output;
        repeated.results.push(repeated.results[0].clone());
        let mut expected_repeated = evidence;
        expected_repeated.push(provenance);
        assert_eq!(
            read_tool_output_semantic_evidence(&repeated, None),
            expected_repeated
        );
    }

    #[test]
    fn recovered_text_preserves_incomplete_selector_status() {
        let mut recovered = selector_result(ToolOutputSelectorStatus::Ok);
        recovered.text = Some("src/lib.rs:10:let stable = compute();".to_string());
        let complete = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 40,
            retained_bytes: 40,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![recovered.clone()],
        };
        let incomplete = ReadToolOutputResult {
            results: vec![
                recovered,
                selector_result(ToolOutputSelectorStatus::AggregateOmitted),
            ],
            ..complete.clone()
        };

        assert_ne!(
            read_tool_output_semantic_evidence(&incomplete, None),
            read_tool_output_semantic_evidence(&complete, None)
        );
    }

    #[test]
    fn disjoint_recovered_fragments_do_not_create_synthetic_semantic_facts() {
        let fragments = [
            "diff --git a/src/lib.rs b/src/lib.rs",
            "@@ -9,0 +10 @@\n+let stable = compute();",
        ];
        let mut results = Vec::new();
        for fragment in fragments {
            let mut result = selector_result(ToolOutputSelectorStatus::Ok);
            result.text = Some(fragment.to_string());
            results.push(result);
        }
        let output = ReadToolOutputResult {
            artifact_id: "artifact".to_string(),
            canonical_sha256: "canonical-revision".to_string(),
            canonical_bytes: 80,
            retained_bytes: 80,
            complete: true,
            unavailable_ranges: Vec::new(),
            results,
        };
        let expected = fragments
            .iter()
            .flat_map(|fragment| semantic_evidence_for_command_output(fragment.as_bytes()))
            .collect::<Vec<_>>();

        let evidence = read_tool_output_semantic_evidence(&output, None);
        assert!(expected.iter().all(|fact| evidence.contains(fact)));
        assert_eq!(
            evidence
                .iter()
                .filter(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
                .count(),
            fragments.len()
        );
        assert_ne!(
            evidence,
            semantic_evidence_for_command_output(fragments.join("\n").as_bytes())
        );
    }

    #[test]
    fn recovered_fragment_identity_includes_its_selector() {
        let mut first = selector_result(ToolOutputSelectorStatus::Ok);
        first.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
        first.text = Some("same recovered fact".to_string());
        let mut second = first.clone();
        second.selector = ToolOutputSelector::Lines { start: 2, end: 2 };
        let output = recovery_output(vec![first, second]);

        let provenance = read_tool_output_semantic_evidence(&output, None)
            .into_iter()
            .filter(|fact| fact.starts_with("artifact-recovery-fragment-v1:"))
            .collect::<Vec<_>>();

        assert_eq!(provenance.len(), 2);
        assert_ne!(provenance[0], provenance[1]);
    }

    #[test]
    fn continuation_stop_is_typed_and_preserves_the_unconsumed_selector() {
        let selector = search_selector(10);
        let initial = recovery_output(vec![continuation_result(
            search_selector(0),
            Some(selector.clone()),
            "first page",
        )]);
        let mut state = RecoveryContinuationState::new(initial, true, usize::MAX);
        state.record_stop(ContinuationStopReason::Budget, Some(selector.clone()));

        let stop = state
            .finish()
            .continuation_stop
            .expect("typed stop receipt");
        assert_eq!(stop.reason, ContinuationStopReason::Budget);
        assert_eq!(stop.selector, Some(selector));
        assert!(stop.resumable);
        assert_eq!(
            stop.message.as_deref(),
            Some(
                "Recovery reached its output budget. Continue with the unconsumed selector in a new call."
            )
        );
        assert_eq!(
            serde_json::to_value(stop).expect("serialize stop")["reason"],
            "budget"
        );
    }

    #[test]
    fn continuation_page_error_preserves_retryability_and_cause() {
        let selector = search_selector(10);
        let cases = [
            (ReadToolOutputError::InvalidArtifactId, false),
            (
                ReadToolOutputError::InvalidRange("invalid continuation range".to_string()),
                false,
            ),
            (ReadToolOutputError::Expired, false),
            (ReadToolOutputError::StillWriting, true),
            (
                ReadToolOutputError::Io("artifact storage unavailable".to_string()),
                false,
            ),
        ];

        for (error, resumable) in cases {
            let expected_message = error.for_model();
            let mut state = RecoveryContinuationState::new(
                recovery_output(vec![selector_result(ToolOutputSelectorStatus::Ok)]),
                false,
                usize::MAX,
            );
            state.record_page_read_error(&error, selector.clone());
            let stop = state
                .finish()
                .continuation_stop
                .expect("page error stop receipt");
            assert_eq!(stop.reason, ContinuationStopReason::PageReadError);
            assert_eq!(stop.selector, Some(selector.clone()));
            assert_eq!(stop.resumable, resumable);
            assert_eq!(stop.message.as_deref(), Some(expected_message.as_str()));
        }
    }

    #[test]
    fn default_range_is_exactly_two_hundred_lines() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: Some(17),
            end_line: None,
            ranges: None,
            max_bytes: None,
        };
        assert_eq!(resolved_line_range(&args).unwrap(), (17, 216));
    }

    #[test]
    fn legacy_single_range_uses_the_canonical_line_invariants() {
        for (start_line, end_line) in [(0, Some(1)), (3, Some(2))] {
            let args = ReadToolOutputArgs {
                artifact_id: uuid::Uuid::now_v7().to_string(),
                selectors: None,
                start_line: Some(start_line),
                end_line,
                ranges: None,
                max_bytes: None,
            };
            assert!(resolved_selectors(&args).is_err());
        }

        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: Some(3),
            end_line: Some(3),
            ranges: None,
            max_bytes: None,
        };
        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![ToolOutputSelector::Lines { start: 3, end: 3 }]
        );
    }

    #[test]
    fn max_bytes_is_legacy_validated_but_not_a_clipping_contract() {
        assert_eq!(resolved_max_bytes(None).unwrap(), 16_384);
        assert_eq!(resolved_max_bytes(Some(1)).unwrap(), 1);
        assert_eq!(resolved_max_bytes(Some(16_384)).unwrap(), 16_384);
        for invalid in [0, 16_385, usize::MAX] {
            assert!(resolved_max_bytes(Some(invalid)).is_err());
        }
    }

    #[test]
    fn read_tool_output_schema_matches_runtime_bounds() {
        let tool = serde_json::to_value(create_read_tool_output_tool())
            .expect("serialize read_tool_output tool");
        let validator = jsonschema::validator_for(&tool["parameters"])
            .expect("compile read_tool_output schema");
        let line_selector = serde_json::json!({
            "kind": "lines",
            "start": 1,
            "end": 1,
        });
        let selector_args = |count: usize| {
            serde_json::json!({
                "artifact_id": "artifact",
                "selectors": vec![line_selector.clone(); count],
            })
        };
        let range_args = |count: usize| {
            serde_json::json!({
                "artifact_id": "artifact",
                "ranges": (1..=count)
                    .map(|line| serde_json::json!({
                        "start_line": line,
                        "end_line": line,
                    }))
                    .collect::<Vec<_>>(),
            })
        };
        let runtime_accepts = |value: &Value| {
            parse_read_tool_output_args(&value.to_string())
                .ok()
                .is_some_and(|args| {
                    resolved_max_bytes(args.max_bytes).is_ok() && resolved_selectors(&args).is_ok()
                })
        };
        let cases = [
            (selector_args(1), true),
            (selector_args(READ_TOOL_OUTPUT_MAX_SELECTORS), true),
            (selector_args(0), false),
            (selector_args(READ_TOOL_OUTPUT_MAX_SELECTORS + 1), false),
            (range_args(1), true),
            (range_args(64), true),
            (range_args(0), false),
            (range_args(65), false),
        ];

        for (arguments, expected) in cases {
            assert_eq!(
                validator.is_valid(&arguments),
                expected,
                "schema verdict for {arguments}"
            );
            assert_eq!(
                runtime_accepts(&arguments),
                expected,
                "runtime verdict for {arguments}"
            );
        }

        // Old callers may still supply max_bytes, but new calls must use the
        // selector bounds advertised by the schema instead of a clipping knob.
        for (max_bytes, runtime_expected) in [
            (0, false),
            (1, true),
            (READ_TOOL_OUTPUT_MAX_BYTES, true),
            (READ_TOOL_OUTPUT_MAX_BYTES + 1, false),
        ] {
            let mut arguments = selector_args(1);
            arguments["max_bytes"] = serde_json::json!(max_bytes);
            assert!(
                !validator.is_valid(&arguments),
                "legacy field is not advertised"
            );
            assert_eq!(
                runtime_accepts(&arguments),
                runtime_expected,
                "legacy runtime verdict for {arguments}"
            );
        }
    }

    #[test]
    fn invalid_recovery_selectors_defer_shape_to_advertised_schema() {
        let error = parse_read_tool_output_args(
            r#"{"artifact_id":"artifact","selector":{"kind":"line","start":1,"end":2}}"#,
        )
        .expect_err("singular selector and line kind must be rejected");
        let FunctionCallError::RespondToModel(message) = error else {
            panic!("parse failures must be returned to the model");
        };

        assert!(message.contains("advertised read_tool_output schema"));
        assert!(!message.contains(r#""selectors""#));
        assert!(!message.contains(r#"{"artifact_id""#));
    }

    #[test]
    fn three_exact_ranges_become_one_bounded_owner_batch() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: None,
            end_line: None,
            ranges: Some(vec![
                ReadToolOutputRangeArgs {
                    start_line: 2,
                    end_line: 4,
                },
                ReadToolOutputRangeArgs {
                    start_line: 11,
                    end_line: 13,
                },
                ReadToolOutputRangeArgs {
                    start_line: 21,
                    end_line: 25,
                },
            ]),
            max_bytes: None,
        };

        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![
                ToolOutputSelector::Lines { start: 2, end: 4 },
                ToolOutputSelector::Lines { start: 11, end: 13 },
                ToolOutputSelector::Lines { start: 21, end: 25 },
            ]
        );
    }

    #[test]
    fn legacy_ranges_are_sorted_merged_and_capped_at_sixteen() {
        let mut args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: None,
            end_line: None,
            ranges: Some(vec![
                ReadToolOutputRangeArgs {
                    start_line: 10,
                    end_line: 12,
                },
                ReadToolOutputRangeArgs {
                    start_line: 2,
                    end_line: 4,
                },
                ReadToolOutputRangeArgs {
                    start_line: 4,
                    end_line: 6,
                },
                ReadToolOutputRangeArgs {
                    start_line: 7,
                    end_line: 9,
                },
            ]),
            max_bytes: None,
        };

        assert_eq!(
            resolved_selectors(&args).unwrap(),
            vec![ToolOutputSelector::Lines { start: 2, end: 12 }]
        );

        args.ranges = Some(
            (1..=READ_TOOL_OUTPUT_MAX_LEGACY_RANGES)
                .map(|line| ReadToolOutputRangeArgs {
                    start_line: line * 2,
                    end_line: line * 2,
                })
                .collect(),
        );
        assert_eq!(
            resolved_selectors(&args).unwrap().len(),
            READ_TOOL_OUTPUT_MAX_LEGACY_RANGES
        );

        args.ranges = Some(
            (1..=READ_TOOL_OUTPUT_MAX_LEGACY_RANGES + 1)
                .map(|line| ReadToolOutputRangeArgs {
                    start_line: line * 2,
                    end_line: line * 2,
                })
                .collect(),
        );
        assert!(resolved_selectors(&args).is_err());

        args.ranges = Some(vec![
            ReadToolOutputRangeArgs {
                start_line: 1,
                end_line: 1_000,
            },
            ReadToolOutputRangeArgs {
                start_line: 2_000,
                end_line: 3_000,
            },
        ]);
        assert!(resolved_selectors(&args).is_err());
    }
}
